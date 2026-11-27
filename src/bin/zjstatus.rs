//! zjstatus as a shared plugin: Zellij runs one instance per session, and every pane showing the
//! bar is a slot of it. Session-wide state (tabs, panes, modes) is kept once; what depends on a
//! bar's configuration (widgets, command results) is kept per configuration, so all bars of a
//! layout share one copy; what depends on the pane (render cache, width) is kept per slot.

use zellij_tile::prelude::*;

use chrono::{DateTime, Local};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
};
use uuid::Uuid;

use zjstatus::{
    config::{ModuleConfig, UpdateEventMask, ZellijState},
    frames, pipe,
    widgets::{
        command::{CommandResult, CommandWidget, PLUGIN_UUID_CONTEXT_KEY},
        datetime::DateTimeWidget,
        mode::ModeWidget,
        notification::{self, NotificationWidget},
        pipe::PipeWidget,
        session::SessionWidget,
        swap_layout::SwapLayoutWidget,
        tabs::TabsWidget,
        widget::Widget,
    },
};

// Matches the old incidental Zellij session scan cadence.
const REFRESH_INTERVAL_SECONDS: f64 = 1.0;

/// How often (once per refresh) to try looking the session name up before giving up.
const SESSION_NAME_LOOKUPS: u8 = 30;

/// A slot's merged plugin configuration; bars with equal configurations share a profile.
type Configuration = BTreeMap<String, String>;

/// What all bars with the same configuration share.
struct Profile {
    /// Parsed once for the frame settings; `None` with `err` set when the configuration is invalid.
    settings: Option<ModuleConfig>,
    err: Option<String>,
    widget_map: BTreeMap<String, Arc<dyn Widget>>,
    focus_cwd_commands: Vec<String>,
    command_results: BTreeMap<String, CommandResult>,
    /// Tags this profile's commands (to route their results back) and lock files.
    plugin_uuid: String,
}

impl Profile {
    fn new(configuration: &Configuration) -> Self {
        let (settings, err) = match ModuleConfig::new(configuration) {
            Ok(settings) => (Some(settings), None),
            Err(e) => (None, Some(format!("{e:?}"))),
        };

        Self {
            settings,
            err,
            widget_map: register_widgets(configuration),
            focus_cwd_commands: zjstatus::widgets::command::focus_cwd_command_names(
                configuration,
            ),
            command_results: BTreeMap::new(),
            plugin_uuid: Uuid::new_v4().to_string(),
        }
    }

    fn frame_config(&self) -> Option<frames::FrameConfig> {
        let settings = self.settings.as_ref()?;
        let config = frames::FrameConfig::new(
            settings.hide_frame_for_single_pane,
            settings.hide_frame_except_for_search,
            settings.hide_frame_except_for_fullscreen,
            settings.hide_frame_except_for_scroll,
            &settings.pane_frame_style,
        );

        (!config.is_disabled()).then_some(config)
    }
}

/// One pane showing the bar.
struct Slot {
    configuration: Configuration,
    /// Each slot parses its own copy: the parts cache their rendered output.
    module_config: Option<ModuleConfig>,
    /// Every event kind since this slot was last rendered, so a slot that stayed hidden
    /// (in an unvisited tab) refreshes all parts those events touched.
    pending_mask: u8,
    cols: usize,
}

#[derive(Default)]
struct State {
    got_permissions: bool,
    requested_permissions: bool,
    pending_events: Vec<(Event, EventContext)>,
    start_time: DateTime<Local>,

    profiles: BTreeMap<Configuration, Profile>,
    slots: BTreeMap<SlotId, Slot>,
    subscribed_to_sessions: bool,

    /// Mode and tabs differ per client (each has its own mode and active tab); events without
    /// a client are kept under `None`.
    modes: HashMap<Option<ClientId>, ModeInfo>,
    tabs: HashMap<Option<ClientId>, Vec<TabInfo>>,
    /// The client whose active tab decides the focused pane (the one that sent tabs last).
    focus_client: Option<ClientId>,
    panes: PaneManifest,
    sessions: Vec<SessionInfo>,
    /// The session's name, for when no ModeUpdate (which normally carries it) has arrived:
    /// Zellij drops events to a shared plugin until its permissions are granted, and sends the
    /// next ModeUpdate only when the mode changes.
    session_name: Option<String>,
    /// Lookups of `session_name` left; the session list is unavailable for the first moments
    /// of a session, so the refresh timer retries.
    session_name_lookups_left: u8,
    pipe_results: BTreeMap<String, String>,
    incoming_notification: Option<notification::Message>,
    focused_pane_id: Option<PaneId>,
    focused_pane_cwd: Option<PathBuf>,
}

#[cfg(not(test))]
register_shared_plugin!(State);

#[cfg(feature = "tracing")]
fn init_tracing() {
    use std::fs::File;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    let file = File::create("/host/.zjstatus.log");
    let file = match file {
        Ok(file) => file,
        Err(error) => panic!("Error: {:?}", error),
    };
    let debug_log = tracing_subscriber::fmt::layer().with_writer(Arc::new(file));

    tracing_subscriber::registry().with(debug_log).init();

    tracing::info!("tracing initialized");
}

impl ZellijSharedPlugin for State {
    fn load(&mut self, _configuration: Configuration) {
        #[cfg(feature = "tracing")]
        init_tracing();

        // SessionUpdate describes every session on the machine and fires whenever any of them
        // changes; only frame hiding reads it, so slot_added subscribes when a bar hides frames.
        subscribe(&[
            EventType::Mouse,
            EventType::ModeUpdate,
            EventType::PaneUpdate,
            EventType::PermissionRequestResult,
            EventType::Timer,
            EventType::TabUpdate,
            EventType::RunCommandResult,
            EventType::CwdChanged,
        ]);
        set_timeout(REFRESH_INTERVAL_SECONDS);

        self.start_time = Local::now();
    }

    fn slot_added(&mut self, slot: zellij_tile::prelude::Slot) {
        let configuration = slot.configuration;
        let profile = self
            .profiles
            .entry(configuration.clone())
            .or_insert_with(|| Profile::new(&configuration));

        // Zellij shows the permission prompt in the pane of the slot that asks. load() runs
        // before any slot exists, so a request from there has no pane to show the prompt in and
        // is never answered; ask from the first slot instead.
        if !self.requested_permissions {
            // we need the ReadApplicationState permission to receive the ModeUpdate and
            // TabUpdate events
            // we need the RunCommands permission to run "cargo test" in a floating window
            request_permission(&[
                PermissionType::ReadApplicationState,
                PermissionType::ChangeApplicationState,
                PermissionType::RunCommands,
            ]);
            self.requested_permissions = true;
        }

        if profile.frame_config().is_some() && !self.subscribed_to_sessions {
            subscribe(&[EventType::SessionUpdate]);
            self.subscribed_to_sessions = true;
        }

        if self.got_permissions {
            let _ = set_selectable_slot(slot.id, false);
        }

        self.slots.insert(
            slot.id,
            Slot {
                module_config: ModuleConfig::new(&configuration).ok(),
                configuration,
                pending_mask: u8::MAX,
                cols: 0,
            },
        );
    }

    fn slot_removed(&mut self, slot_id: SlotId) {
        let Some(slot) = self.slots.remove(&slot_id) else {
            return;
        };

        let still_used = self
            .slots
            .values()
            .any(|s| s.configuration == slot.configuration);
        if !still_used {
            self.profiles.remove(&slot.configuration);
        }
    }

    fn pipe(&mut self, pipe_message: PipeMessage, _context: EventContext) -> RenderResponse {
        let input = match pipe_message.source {
            PipeSource::Cli(_) | PipeSource::Plugin(_) | PipeSource::Keybind => {
                pipe_message.payload
            }
            // Zellij 0.46 answers prompts through pipes; zjstatus shows none.
            PipeSource::PromptRequest { .. } => None,
        };

        match input {
            Some(input) if self.apply_pipe(&input) => RenderResponse::All,
            _ => RenderResponse::Nothing,
        }
    }

    #[tracing::instrument(skip_all, fields(event_type))]
    fn update(&mut self, event: Event, context: EventContext) -> RenderResponse {
        if let Event::PermissionRequestResult(PermissionStatus::Granted) = event {
            self.got_permissions = true;

            let mut render = RenderResponse::Nothing;
            for (event, context) in std::mem::take(&mut self.pending_events) {
                tracing::debug!("processing cached event");
                render = render.merge(self.handle_event(event, context));
            }

            return render.merge(self.handle_event(event, context));
        }

        if !self.got_permissions {
            tracing::debug!("caching event");
            self.pending_events.push((event, context));

            return RenderResponse::Nothing;
        }

        self.handle_event(event, context)
    }

    #[tracing::instrument(skip_all)]
    fn render(&mut self, _rows: usize, cols: usize, slot_id: SlotId, client_id: ClientId) {
        if !self.got_permissions {
            return;
        }

        let Some(slot) = self.slots.get(&slot_id) else {
            return;
        };
        let Some(profile) = self.profiles.get(&slot.configuration) else {
            return;
        };

        if let Some(err) = &profile.err {
            println!("Error: {}", err);

            return;
        }

        // A new width invalidates every cached part.
        let mask = if slot.cols == cols {
            slot.pending_mask
        } else {
            u8::MAX
        };
        let state = self.zellij_state(profile, Some(client_id), cols, mask);
        let widget_map = profile.widget_map.clone();

        let Some(slot) = self.slots.get_mut(&slot_id) else {
            return;
        };
        slot.pending_mask = 0;
        slot.cols = cols;

        if let Some(module_config) = slot.module_config.as_mut() {
            print!("{}", module_config.render_bar(state, widget_map));
        }
    }
}

impl State {
    fn mode_of(&self, client: Option<ClientId>) -> ModeInfo {
        let mut mode = self
            .modes
            .get(&client)
            .or_else(|| self.modes.values().next())
            .cloned()
            .unwrap_or_default();

        if mode.session_name.is_none() {
            mode.session_name = self.session_name.clone();
        }

        mode
    }

    /// Looks the session's name up, as the ModeUpdate carrying it may have been dropped.
    /// Returns whether it found one. The session list errors with "Session-scan state not
    /// initialized" until Zellij's first session scan, so callers retry.
    fn fetch_session_name(&mut self) -> bool {
        if self.session_name.is_some() || self.session_name_lookups_left == 0 {
            return false;
        }
        self.session_name_lookups_left -= 1;

        match get_session_list() {
            Ok(snapshot) => {
                if let Some(current) = snapshot.live_sessions.iter().find(|s| s.is_current_session)
                {
                    self.session_name = Some(current.name.clone());
                    self.mark_all(UpdateEventMask::Mode);
                    return true;
                }
            }
            Err(e) => tracing::debug!("could not get the session list: {e}"),
        }

        false
    }

    fn tabs_of(&self, client: Option<ClientId>) -> Vec<TabInfo> {
        self.tabs
            .get(&client)
            .or_else(|| self.tabs.values().next())
            .cloned()
            .unwrap_or_default()
    }

    /// The state widgets render from, as one zjstatus instance used to see it.
    fn zellij_state(
        &self,
        profile: &Profile,
        client: Option<ClientId>,
        cols: usize,
        cache_mask: u8,
    ) -> ZellijState {
        ZellijState {
            cols,
            command_results: profile.command_results.clone(),
            pipe_results: self.pipe_results.clone(),
            mode: self.mode_of(client),
            panes: self.panes.clone(),
            plugin_uuid: profile.plugin_uuid.clone(),
            tabs: self.tabs_of(client),
            sessions: self.sessions.clone(),
            start_time: self.start_time,
            incoming_notification: self.incoming_notification.clone(),
            cache_mask,
            focused_pane_id: self.focused_pane_id,
            focused_pane_cwd: self.focused_pane_cwd.clone(),
        }
    }

    /// Records that `mask` changed for every slot, rendered now or the next time it is visible.
    fn mark_all(&mut self, mask: UpdateEventMask) {
        let mask = mask as u8;
        for slot in self.slots.values_mut() {
            slot.pending_mask |= mask;
        }
    }

    fn render_for(client: Option<ClientId>) -> RenderResponse {
        match client {
            Some(client_id) => RenderResponse::Client(client_id),
            None => RenderResponse::All,
        }
    }

    /// Runs the pipe protocol against each profile: commands to rerun are per profile, pipe
    /// results and notifications are shared by all bars.
    fn apply_pipe(&mut self, input: &str) -> bool {
        let mut should_render = false;
        let keys: Vec<Configuration> = self.profiles.keys().cloned().collect();

        for key in keys {
            let Some(profile) = self.profiles.get(&key) else {
                continue;
            };
            let mut state = self.zellij_state(profile, None, 0, 0);
            should_render |= pipe::parse_protocol(&mut state, input);

            self.pipe_results = state.pipe_results;
            self.incoming_notification = state.incoming_notification;
            if let Some(profile) = self.profiles.get_mut(&key) {
                profile.command_results = state.command_results;
            }
        }

        should_render
    }

    /// Frame hiding acts on the session's panes, so the first bar configuration that asks for
    /// it decides. With one instance per session it runs once, like zjframes.
    fn hide_frames(&self, tabs: &[TabInfo], panes: &PaneManifest) {
        let Some(config) = self.profiles.values().find_map(Profile::frame_config) else {
            return;
        };

        frames::hide_frames_conditionally(
            &config,
            tabs,
            panes,
            &self.mode_of(self.focus_client),
            get_plugin_ids(),
            true,
        );
    }

    fn update_focused_pane(&mut self) {
        let tabs = self.tabs_of(self.focus_client);
        let active_tab = tabs.iter().find(|t| t.active);

        let new_id = active_tab
            .and_then(|tab| self.panes.panes.get(&tab.position))
            .and_then(|panes| panes.iter().find(|p| p.is_focused && !p.is_plugin))
            .map(|p| PaneId::Terminal(p.id));

        if new_id == self.focused_pane_id {
            return;
        }

        self.focused_pane_id = new_id;

        let new_cwd = match new_id {
            Some(pane_id) => match get_pane_cwd(pane_id) {
                Ok(cwd) => Some(cwd),
                Err(e) => {
                    tracing::debug!("could not get pane cwd: {e}");
                    None
                }
            },
            None => None,
        };

        self.set_focused_pane_cwd(new_cwd);
    }

    fn set_focused_pane_cwd(&mut self, new_cwd: Option<PathBuf>) -> bool {
        if new_cwd == self.focused_pane_cwd {
            return false;
        }

        self.focused_pane_cwd = new_cwd;

        let mut invalidated = false;
        for profile in self.profiles.values_mut() {
            if profile.focus_cwd_commands.is_empty() {
                continue;
            }

            let mut state = ZellijState {
                command_results: std::mem::take(&mut profile.command_results),
                plugin_uuid: profile.plugin_uuid.clone(),
                ..ZellijState::default()
            };
            for name in &profile.focus_cwd_commands {
                pipe::invalidate_command_result(&mut state, name);
                zjstatus::widgets::command::release_command_lock(&state, name);
            }
            profile.command_results = state.command_results;
            invalidated = true;
        }

        invalidated
    }

    fn handle_event(&mut self, event: Event, context: EventContext) -> RenderResponse {
        match event {
            Event::Mouse(mouse_info) => {
                tracing::Span::current().record("event_type", "Event::Mouse");
                tracing::debug!(mouse = ?mouse_info);

                let Some(slot_id) = context.slot_id else {
                    return RenderResponse::Nothing;
                };
                let Some(slot) = self.slots.get(&slot_id) else {
                    return RenderResponse::Nothing;
                };
                let Some(profile) = self.profiles.get(&slot.configuration) else {
                    return RenderResponse::Nothing;
                };
                let state = self.zellij_state(profile, context.client_id, slot.cols, 0);
                let widget_map = profile.widget_map.clone();

                if let Some(module_config) = self
                    .slots
                    .get_mut(&slot_id)
                    .and_then(|slot| slot.module_config.as_mut())
                {
                    module_config.handle_mouse_action(state, mouse_info, widget_map);
                }

                RenderResponse::Nothing
            }
            Event::ModeUpdate(mode_info) => {
                tracing::Span::current().record("event_type", "Event::ModeUpdate");
                tracing::debug!(mode = ?mode_info.mode);

                if mode_info.session_name.is_some() {
                    self.session_name = mode_info.session_name.clone();
                }
                self.modes.insert(context.client_id, mode_info);
                self.mark_all(UpdateEventMask::Mode);

                Self::render_for(context.client_id)
            }
            Event::PaneUpdate(pane_info) => {
                tracing::Span::current().record("event_type", "Event::PaneUpdate");
                tracing::debug!(pane_count = ?pane_info.panes.len());

                self.hide_frames(&self.tabs_of(self.focus_client), &pane_info);

                self.panes = pane_info;
                self.mark_all(UpdateEventMask::Tab);
                self.update_focused_pane();

                RenderResponse::All
            }
            Event::CwdChanged(pane_id, cwd, _clients) => {
                tracing::Span::current().record("event_type", "Event::CwdChanged");
                tracing::debug!(pane_id = ?pane_id, cwd = ?cwd);

                if Some(pane_id) == self.focused_pane_id && self.set_focused_pane_cwd(Some(cwd)) {
                    self.mark_all(UpdateEventMask::Command);
                    return RenderResponse::All;
                }

                RenderResponse::Nothing
            }
            Event::PermissionRequestResult(result) => {
                tracing::Span::current().record("event_type", "Event::PermissionRequestResult");
                tracing::debug!(result = ?result);

                for slot_id in self.slots.keys() {
                    let _ = set_selectable_slot(*slot_id, false);
                }

                if matches!(result, PermissionStatus::Granted) {
                    self.session_name_lookups_left = SESSION_NAME_LOOKUPS;
                    self.fetch_session_name();
                }

                RenderResponse::All
            }
            Event::RunCommandResult(exit_code, stdout, stderr, context) => {
                tracing::Span::current().record("event_type", "Event::RunCommandResult");
                tracing::debug!(exit_code = ?exit_code, context = ?context);

                let (Some(name), Some(uuid)) =
                    (context.get("name"), context.get(PLUGIN_UUID_CONTEXT_KEY))
                else {
                    return RenderResponse::Nothing;
                };
                let Some((key, profile)) = self
                    .profiles
                    .iter_mut()
                    .find(|(_, p)| &p.plugin_uuid == uuid)
                else {
                    return RenderResponse::Nothing;
                };

                if profile.focus_cwd_commands.iter().any(|n| n == name)
                    && context.get("cwd").map(PathBuf::from) != self.focused_pane_cwd
                {
                    tracing::debug!("discarding stale command result for {name}");
                    return RenderResponse::Nothing;
                }

                profile.command_results.insert(
                    name.to_owned(),
                    CommandResult {
                        exit_code,
                        stdout: String::from_utf8(stdout).unwrap_or_default(),
                        stderr: String::from_utf8(stderr).unwrap_or_default(),
                        context: context.clone(),
                    },
                );

                let key = key.clone();
                let mut slot_ids = vec![];
                for (slot_id, slot) in self.slots.iter_mut() {
                    if slot.configuration == key {
                        slot.pending_mask |= UpdateEventMask::Command as u8;
                        slot_ids.push(*slot_id);
                    }
                }

                RenderResponse::Slots(slot_ids)
            }
            Event::SessionUpdate(session_info, _) => {
                tracing::Span::current().record("event_type", "Event::SessionUpdate");

                if let Some(current_session) = session_info.iter().find(|s| s.is_current_session) {
                    self.hide_frames(&current_session.tabs, &current_session.panes);
                }

                self.sessions = session_info;
                self.mark_all(UpdateEventMask::Session);

                RenderResponse::All
            }
            Event::TabUpdate(tab_info) => {
                tracing::Span::current().record("event_type", "Event::TabUpdate");
                tracing::debug!(tab_count = ?tab_info.len());

                self.tabs.insert(context.client_id, tab_info);
                self.focus_client = context.client_id;
                self.mark_all(UpdateEventMask::Tab);

                Self::render_for(context.client_id)
            }
            Event::Timer(_) => {
                tracing::Span::current().record("event_type", "Event::Timer");
                set_timeout(REFRESH_INTERVAL_SECONDS);
                self.fetch_session_name();

                RenderResponse::All
            }
            _ => RenderResponse::Nothing,
        }
    }
}

fn register_widgets(configuration: &Configuration) -> BTreeMap<String, Arc<dyn Widget>> {
    let mut widget_map = BTreeMap::<String, Arc<dyn Widget>>::new();

    widget_map.insert(
        "command".to_owned(),
        Arc::new(CommandWidget::new(configuration)),
    );
    widget_map.insert(
        "datetime".to_owned(),
        Arc::new(DateTimeWidget::new(configuration)),
    );
    widget_map.insert("pipe".to_owned(), Arc::new(PipeWidget::new(configuration)));
    widget_map.insert(
        "swap_layout".to_owned(),
        Arc::new(SwapLayoutWidget::new(configuration)),
    );
    widget_map.insert("mode".to_owned(), Arc::new(ModeWidget::new(configuration)));
    widget_map.insert(
        "session".to_owned(),
        Arc::new(SessionWidget::new(configuration)),
    );
    widget_map.insert("tabs".to_owned(), Arc::new(TabsWidget::new(configuration)));
    widget_map.insert(
        "notifications".to_owned(),
        Arc::new(NotificationWidget::new(configuration)),
    );

    tracing::debug!("registered widgets: {:?}", widget_map.keys());

    widget_map
}

#[cfg(test)]
mod test {
    use super::*;

    fn state_with_slots(configurations: &[Configuration]) -> State {
        let mut state = State::default();
        for (id, configuration) in configurations.iter().enumerate() {
            state.slot_added(zellij_tile::prelude::Slot {
                id: id as SlotId,
                kind: SlotKind::Pane,
                configuration: configuration.clone(),
            });
        }
        state
    }

    fn configuration(format_left: &str) -> Configuration {
        BTreeMap::from([("format_left".to_owned(), format_left.to_owned())])
    }

    #[test]
    fn permissions_are_requested_once_a_slot_exists() {
        let mut state = State::default();
        state.load(Configuration::default());
        assert!(!state.requested_permissions);

        state.slot_added(zellij_tile::prelude::Slot {
            id: 0,
            kind: SlotKind::Pane,
            configuration: configuration("{tabs}"),
        });
        assert!(state.requested_permissions);
    }

    #[test]
    fn bars_with_the_same_configuration_share_a_profile() {
        let state = state_with_slots(&[
            configuration("{tabs}"),
            configuration("{tabs}"),
            configuration("{mode}"),
        ]);

        assert_eq!(state.slots.len(), 3);
        assert_eq!(state.profiles.len(), 2);
    }

    #[test]
    fn removing_the_last_slot_of_a_configuration_drops_its_profile() {
        let mut state = state_with_slots(&[configuration("{tabs}"), configuration("{mode}")]);

        state.slot_removed(1);

        assert_eq!(state.slots.len(), 1);
        assert_eq!(state.profiles.len(), 1);
    }

    #[test]
    fn hidden_slots_collect_every_change_until_rendered() {
        let mut state = state_with_slots(&[configuration("{tabs}")]);
        state.slots.get_mut(&0).unwrap().pending_mask = 0;

        state.mark_all(UpdateEventMask::Mode);
        state.mark_all(UpdateEventMask::Session);

        assert_eq!(
            state.slots[&0].pending_mask,
            UpdateEventMask::Mode as u8 | UpdateEventMask::Session as u8
        );
    }

    #[test]
    fn modes_and_tabs_are_kept_per_client() {
        let mut state = state_with_slots(&[configuration("{tabs}")]);
        state.got_permissions = true;
        let tab = |name: &str| TabInfo {
            name: name.to_owned(),
            active: true,
            ..TabInfo::default()
        };

        let context = |client_id| EventContext {
            slot_id: None,
            client_id: Some(client_id),
        };
        state.handle_event(Event::TabUpdate(vec![tab("one")]), context(1));
        state.handle_event(Event::TabUpdate(vec![tab("two")]), context(2));

        assert_eq!(state.tabs_of(Some(1))[0].name, "one");
        assert_eq!(state.tabs_of(Some(2))[0].name, "two");
    }

    #[test]
    fn falls_back_to_the_looked_up_session_name() {
        let mut state = state_with_slots(&[configuration("{session}")]);
        state.session_name = Some("looked-up".to_owned());

        assert_eq!(state.mode_of(Some(1)).session_name.as_deref(), Some("looked-up"));

        let mode = ModeInfo {
            session_name: Some("from-mode-update".to_owned()),
            ..ModeInfo::default()
        };
        let context = EventContext {
            slot_id: None,
            client_id: Some(1),
        };
        state.handle_event(Event::ModeUpdate(mode), context);

        assert_eq!(
            state.mode_of(Some(1)).session_name.as_deref(),
            Some("from-mode-update")
        );
        assert_eq!(state.session_name.as_deref(), Some("from-mode-update"));
    }

    #[test]
    fn set_focused_pane_cwd_only_invalidates_on_change() {
        let mut state = state_with_slots(&[BTreeMap::from([
            ("format_left".to_owned(), "{command_branch}".to_owned()),
            ("command_branch_command".to_owned(), "git branch".to_owned()),
            ("command_branch_cwd".to_owned(), "{focused_pane_cwd}".to_owned()),
        ])]);
        let profile = state.profiles.values_mut().next().unwrap();
        assert_eq!(profile.focus_cwd_commands, vec!["command_branch".to_owned()]);
        profile.command_results.insert(
            "command_branch".to_owned(),
            CommandResult {
                context: BTreeMap::from([(
                    "timestamp".to_owned(),
                    Local::now()
                        .format(zjstatus::widgets::command::TIMESTAMP_FORMAT)
                        .to_string(),
                )]),
                ..CommandResult::default()
            },
        );
        state.focused_pane_cwd = Some(PathBuf::from("/tmp"));

        let timestamp = |state: &State| {
            state.profiles.values().next().unwrap().command_results["command_branch"].context
                ["timestamp"]
                .clone()
        };
        let original_timestamp = timestamp(&state);

        assert!(!state.set_focused_pane_cwd(Some(PathBuf::from("/tmp"))));
        assert_eq!(timestamp(&state), original_timestamp);

        assert!(state.set_focused_pane_cwd(Some(PathBuf::from("/var"))));
        assert_eq!(state.focused_pane_cwd, Some(PathBuf::from("/var")));
        assert_ne!(timestamp(&state), original_timestamp);
    }
}
