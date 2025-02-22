#[cfg(feature = "persistent_world")]
use crate::TerrainPersistence;
use crate::{client::Client, presence::Presence, Settings};
use common::{
    comp::{
        Admin, AdminRole, CanBuild, ControlEvent, Controller, ForceUpdate, Health, Ori, Player,
        Pos, SkillSet, Vel,
    },
    event::{EventBus, ServerEvent},
    link::Is,
    mounting::Rider,
    resources::PlayerPhysicsSettings,
    terrain::TerrainGrid,
    vol::ReadVol,
};
use common_ecs::{Job, Origin, Phase, System};
use common_net::msg::{ClientGeneral, PresenceKind, ServerGeneral};
use common_state::{BlockChange, BuildAreas};
use specs::{Entities, Join, Read, ReadExpect, ReadStorage, Write, WriteStorage};
use std::time::Instant;
use tracing::{debug, trace, warn};
use vek::*;

#[cfg(feature = "persistent_world")]
pub type TerrainPersistenceData<'a> = Option<Write<'a, TerrainPersistence>>;
#[cfg(not(feature = "persistent_world"))]
pub type TerrainPersistenceData<'a> = ();

impl Sys {
    #[allow(clippy::too_many_arguments)]
    fn handle_client_in_game_msg(
        server_emitter: &mut common::event::Emitter<'_, ServerEvent>,
        entity: specs::Entity,
        client: &Client,
        maybe_presence: &mut Option<&mut Presence>,
        terrain: &ReadExpect<'_, TerrainGrid>,
        can_build: &ReadStorage<'_, CanBuild>,
        is_rider: &ReadStorage<'_, Is<Rider>>,
        force_updates: &ReadStorage<'_, ForceUpdate>,
        skill_sets: &mut WriteStorage<'_, SkillSet>,
        healths: &ReadStorage<'_, Health>,
        block_changes: &mut Write<'_, BlockChange>,
        positions: &mut WriteStorage<'_, Pos>,
        velocities: &mut WriteStorage<'_, Vel>,
        orientations: &mut WriteStorage<'_, Ori>,
        controllers: &mut WriteStorage<'_, Controller>,
        settings: &Read<'_, Settings>,
        build_areas: &Read<'_, BuildAreas>,
        player_physics_settings: &mut Write<'_, PlayerPhysicsSettings>,
        _terrain_persistence: &mut TerrainPersistenceData<'_>,
        maybe_player: &Option<&Player>,
        maybe_admin: &Option<&Admin>,
        time_for_vd_changes: Instant,
        msg: ClientGeneral,
    ) -> Result<(), crate::error::Error> {
        let presence = match maybe_presence.as_deref_mut() {
            Some(g) => g,
            None => {
                debug!(?entity, "client is not in_game, ignoring msg");
                trace!(?msg, "ignored msg content");
                return Ok(());
            },
        };
        match msg {
            // Go back to registered state (char selection screen)
            ClientGeneral::ExitInGame => {
                server_emitter.emit(ServerEvent::ExitIngame { entity });
                client.send(ServerGeneral::ExitInGameSuccess)?;
                *maybe_presence = None;
            },
            ClientGeneral::SetViewDistance(view_distances) => {
                let clamped_vds = view_distances.clamp(settings.max_view_distance);

                presence.terrain_view_distance.set_target(clamped_vds.terrain, time_for_vd_changes);
                presence.entity_view_distance.set_target(clamped_vds.entity, time_for_vd_changes);

                // Correct client if its requested VD is too high.
                if view_distances.terrain != clamped_vds.terrain {
                    client.send(ServerGeneral::SetViewDistance(clamped_vds.terrain))?;
                }
            },
            ClientGeneral::ControllerInputs(inputs) => {
                if presence.kind.controlling_char() {
                    if let Some(controller) = controllers.get_mut(entity) {
                        controller.inputs.update_with_new(*inputs);
                    }
                }
            },
            ClientGeneral::ControlEvent(event) => {
                if presence.kind.controlling_char() {
                    // Skip respawn if client entity is alive
                    if let ControlEvent::Respawn = event {
                        if healths.get(entity).map_or(true, |h| !h.is_dead) {
                            //Todo: comment why return!
                            return Ok(());
                        }
                    }
                    if let Some(controller) = controllers.get_mut(entity) {
                        controller.push_event(event);
                    }
                }
            },
            ClientGeneral::ControlAction(event) => {
                if presence.kind.controlling_char() {
                    if let Some(controller) = controllers.get_mut(entity) {
                        controller.push_action(event);
                    }
                }
            },
            ClientGeneral::PlayerPhysics { pos, vel, ori, force_counter } => {
                let player_physics_setting = maybe_player.map(|p| {
                    player_physics_settings
                        .settings
                        .entry(p.uuid())
                        .or_default()
                });

                if presence.kind.controlling_char()
                    && force_updates.get(entity).map_or(true, |force_update| force_update.counter() == force_counter)
                    && healths.get(entity).map_or(true, |h| !h.is_dead)
                    && is_rider.get(entity).is_none()
                    && player_physics_setting
                        .as_ref()
                        .map_or(true, |s| s.client_authoritative())
                {
                    enum Rejection {
                        TooFar { old: Vec3<f32>, new: Vec3<f32> },
                        TooFast { vel: Vec3<f32> },
                    }

                    let rejection = if maybe_admin.is_some() {
                        None
                    } else if let Some(mut setting) = player_physics_setting {
                        // If we detect any thresholds being exceeded, force server-authoritative
                        // physics for that player. This doesn't detect subtle hacks, but it
                        // prevents blatant ones and forces people to not debug physics hacks on the
                        // live server (and also mitigates some floating-point overflow crashes)
                        let rejection = None
                            // Check position
                            .or_else(|| {
                                if let Some(prev_pos) = positions.get(entity) {
                                    if prev_pos.0.distance_squared(pos.0) > (500.0f32).powf(2.0) {
                                        Some(Rejection::TooFar { old: prev_pos.0, new: pos.0 })
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                            // Check velocity
                            .or_else(|| {
                                if vel.0.magnitude_squared() > (500.0f32).powf(2.0) {
                                    Some(Rejection::TooFast { vel: vel.0 })
                                } else {
                                    None
                                }
                            });

                        // Force a client-side physics update if rejectable physics data is
                        // received.
                        if rejection.is_some() {
                            // We skip this for `TooFar` because false positives can occur when
                            // using server-side teleportation commands
                            // that the client doesn't know about (leading to the client sending
                            // physics state that disagree with the server). In the future,
                            // client-authoritative physics will be gone
                            // and this will no longer be necessary.
                            setting.server_force =
                                !matches!(rejection, Some(Rejection::TooFar { .. })); // true;
                        }

                        rejection
                    } else {
                        None
                    };

                    match rejection {
                        Some(Rejection::TooFar { old, new }) => warn!(
                            "Rejected player physics update (new position {:?} is too far from \
                             old position {:?})",
                            new, old
                        ),
                        Some(Rejection::TooFast { vel }) => warn!(
                            "Rejected player physics update (new velocity {:?} is too fast)",
                            vel
                        ),
                        None => {
                            // Don't insert unless the component already exists
                            let _ = positions.get_mut(entity).map(|p| *p = pos);
                            let _ = velocities.get_mut(entity).map(|v| *v = vel);
                            let _ = orientations.get_mut(entity).map(|o| *o = ori);
                        },
                    }
                }
            },
            ClientGeneral::BreakBlock(pos) => {
                if let Some(comp_can_build) = can_build.get(entity) {
                    if comp_can_build.enabled {
                        for area in comp_can_build.build_areas.iter() {
                            if let Some(old_block) = build_areas
                                .areas()
                                .get(*area)
                                // TODO: Make this an exclusive check on the upper bound of the AABB
                                // Vek defaults to inclusive which is not optimal
                                .filter(|aabb| aabb.contains_point(pos))
                                .and_then(|_| terrain.get(pos).ok())
                            {
                                let new_block = old_block.into_vacant();
                                let _was_set = block_changes.try_set(pos, new_block).is_some();
                                #[cfg(feature = "persistent_world")]
                                if _was_set {
                                    if let Some(terrain_persistence) = _terrain_persistence.as_mut()
                                    {
                                        terrain_persistence.set_block(pos, new_block);
                                    }
                                }
                            }
                        }
                    }
                }
            },
            ClientGeneral::PlaceBlock(pos, new_block) => {
                if let Some(comp_can_build) = can_build.get(entity) {
                    if comp_can_build.enabled {
                        for area in comp_can_build.build_areas.iter() {
                            if build_areas
                                .areas()
                                .get(*area)
                                // TODO: Make this an exclusive check on the upper bound of the AABB
                                // Vek defaults to inclusive which is not optimal
                                .filter(|aabb| aabb.contains_point(pos))
                                .is_some()
                            {
                                let _was_set = block_changes.try_set(pos, new_block).is_some();
                                #[cfg(feature = "persistent_world")]
                                if _was_set {
                                    if let Some(terrain_persistence) = _terrain_persistence.as_mut()
                                    {
                                        terrain_persistence.set_block(pos, new_block);
                                    }
                                }
                            }
                        }
                    }
                }
            },
            ClientGeneral::UnlockSkill(skill) => {
                skill_sets
                    .get_mut(entity)
                    .map(|mut skill_set| skill_set.unlock_skill(skill));
            },
            ClientGeneral::UnlockSkillGroup(skill_group_kind) => {
                skill_sets
                    .get_mut(entity)
                    .map(|mut skill_set| skill_set.unlock_skill_group(skill_group_kind));
            },
            ClientGeneral::RequestSiteInfo(id) => {
                server_emitter.emit(ServerEvent::RequestSiteInfo { entity, id });
            },
            ClientGeneral::RequestPlayerPhysics {
                server_authoritative,
            } => {
                let player_physics_setting = maybe_player.map(|p| {
                    player_physics_settings
                        .settings
                        .entry(p.uuid())
                        .or_default()
                });
                if let Some(setting) = player_physics_setting {
                    setting.client_optin = server_authoritative;
                }
            },
            ClientGeneral::RequestLossyTerrainCompression {
                lossy_terrain_compression,
            } => {
                presence.lossy_terrain_compression = lossy_terrain_compression;
            },
            ClientGeneral::AcknowledgePersistenceLoadError => {
                skill_sets
                    .get_mut(entity)
                    .map(|mut skill_set| skill_set.persistence_load_error = None);
            },
            ClientGeneral::UpdateMapMarker(update) => {
                server_emitter.emit(ServerEvent::UpdateMapMarker { entity, update });
            },
            ClientGeneral::SpectatePosition(pos) => {
                if let Some(admin) = maybe_admin && admin.0 >= AdminRole::Moderator && presence.kind == PresenceKind::Spectator {
                    if let Some(position) = positions.get_mut(entity) {
                        position.0 = pos;
                    }
                }
            },
            ClientGeneral::RequestCharacterList
            | ClientGeneral::CreateCharacter { .. }
            | ClientGeneral::EditCharacter { .. }
            | ClientGeneral::DeleteCharacter(_)
            | ClientGeneral::Character(_, _)
            | ClientGeneral::Spectate(_)
            | ClientGeneral::TerrainChunkRequest { .. }
            | ClientGeneral::LodZoneRequest { .. }
            | ClientGeneral::ChatMsg(_)
            | ClientGeneral::Command(..)
            | ClientGeneral::Terminate => {
                debug!("Kicking possibly misbehaving client due to invalid client in game request");
                server_emitter.emit(ServerEvent::ClientDisconnect(
                    entity,
                    common::comp::DisconnectReason::NetworkError,
                ));
            },
        }
        Ok(())
    }
}

/// This system will handle new messages from clients
#[derive(Default)]
pub struct Sys;
impl<'a> System<'a> for Sys {
    #[allow(clippy::type_complexity)]
    type SystemData = (
        Entities<'a>,
        Read<'a, EventBus<ServerEvent>>,
        ReadExpect<'a, TerrainGrid>,
        ReadStorage<'a, CanBuild>,
        ReadStorage<'a, ForceUpdate>,
        ReadStorage<'a, Is<Rider>>,
        WriteStorage<'a, SkillSet>,
        ReadStorage<'a, Health>,
        Write<'a, BlockChange>,
        WriteStorage<'a, Pos>,
        WriteStorage<'a, Vel>,
        WriteStorage<'a, Ori>,
        WriteStorage<'a, Presence>,
        WriteStorage<'a, Client>,
        WriteStorage<'a, Controller>,
        Read<'a, Settings>,
        Read<'a, BuildAreas>,
        Write<'a, PlayerPhysicsSettings>,
        TerrainPersistenceData<'a>,
        ReadStorage<'a, Player>,
        ReadStorage<'a, Admin>,
    );

    const NAME: &'static str = "msg::in_game";
    const ORIGIN: Origin = Origin::Server;
    const PHASE: Phase = Phase::Create;

    fn run(
        _job: &mut Job<Self>,
        (
            entities,
            server_event_bus,
            terrain,
            can_build,
            force_updates,
            is_rider,
            mut skill_sets,
            healths,
            mut block_changes,
            mut positions,
            mut velocities,
            mut orientations,
            mut presences,
            mut clients,
            mut controllers,
            settings,
            build_areas,
            mut player_physics_settings,
            mut terrain_persistence,
            players,
            admins,
        ): Self::SystemData,
    ) {
        let mut server_emitter = server_event_bus.emitter();

        let time_for_vd_changes = Instant::now();

        for (entity, client, mut maybe_presence, player, maybe_admin) in (
            &entities,
            &mut clients,
            (&mut presences).maybe(),
            players.maybe(),
            admins.maybe(),
        )
            .join()
        {
            // If an `ExitInGame` message is received this is set to `None` allowing further
            // ingame messages to be ignored.
            let mut clearable_maybe_presence = maybe_presence.as_deref_mut();
            let _ = super::try_recv_all(client, 2, |client, msg| {
                Self::handle_client_in_game_msg(
                    &mut server_emitter,
                    entity,
                    client,
                    &mut clearable_maybe_presence,
                    &terrain,
                    &can_build,
                    &is_rider,
                    &force_updates,
                    &mut skill_sets,
                    &healths,
                    &mut block_changes,
                    &mut positions,
                    &mut velocities,
                    &mut orientations,
                    &mut controllers,
                    &settings,
                    &build_areas,
                    &mut player_physics_settings,
                    &mut terrain_persistence,
                    &player,
                    &maybe_admin,
                    time_for_vd_changes,
                    msg,
                )
            });

            // Ensure deferred view distance changes are applied (if the
            // requsite time has elapsed).
            if let Some(presence) = maybe_presence {
                presence.terrain_view_distance.update(time_for_vd_changes);
                presence.entity_view_distance.update(time_for_vd_changes);
            }
        }
    }
}
