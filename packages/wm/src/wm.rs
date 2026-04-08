use std::time::{Duration, Instant};
use anyhow::{bail, Context};
use tokio::sync::mpsc::{self};
use tracing::warn;
use uuid::Uuid;
#[cfg(target_os = "windows")]
use wm_common::TitleBarVisibility;
use wm_common::{
  FloatingStateConfig, FullscreenStateConfig, InvokeCommand, WindowState,
  WmEvent,
};
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;
use wm_platform::{
  Dispatcher, LengthValue, PlatformEvent, RectDelta, WindowEvent,
};

use crate::{
  commands::{
    container::{
      focus_container_by_id, focus_in_direction, set_tiling_direction,
      toggle_tiling_direction,
    },
    general::{
      cycle_focus, disable_binding_mode, enable_binding_mode,
      platform_sync, reload_config, shell_exec, toggle_pause,
    },
    monitor::focus_monitor,
    window::{
      ignore_window, move_window_in_direction, move_window_to_workspace,
      resize_window, set_window_position, set_window_size,
      update_window_state, WindowPositionTarget,
    },
    workspace::{
      focus_workspace, move_workspace_in_direction,
      update_workspace_config,
    },
  },
  events::{
    handle_display_settings_changed, handle_mouse_move,
    handle_window_destroyed, handle_window_focused, handle_window_hidden,
    handle_window_minimize_ended, handle_window_minimized,
    handle_window_moved_or_resized, handle_window_shown,
    handle_window_title_changed,
  },
  ipc_server::IpcServer,
  models::{Container, WorkspaceTarget},
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub struct WindowManager {
  pub event_rx: mpsc::UnboundedReceiver<WmEvent>,
  pub exit_rx: mpsc::UnboundedReceiver<()>,
  pub state: WmState,

  // 🔥 DEBOUNCE STATE
  last_retile: Instant,
  retile_pending: bool,
}

impl WindowManager {
  pub fn new(
    config: &mut UserConfig,
    dispatcher: Dispatcher,
  ) -> anyhow::Result<Self> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (exit_tx, exit_rx) = mpsc::unbounded_channel();

    let mut state = WmState::new(dispatcher, event_tx, exit_tx);
    state.populate(config)?;

    Ok(Self {
      event_rx,
      exit_rx,
      state,

      // 🔥 INIT DEBOUNCE
      last_retile: Instant::now(),
      retile_pending: false,
    })
  }

  pub fn process_event(
    &mut self,
    event: PlatformEvent,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    let state = &mut self.state;

    match event {
      PlatformEvent::DisplaySettingsChanged => {
        handle_display_settings_changed(state, config)
      }
      PlatformEvent::Keybinding(keybinding_event) => {
        let commands = config
          .active_keybinding_configs(
            &self.state.binding_modes,
            self.state.is_paused,
          )
          .find(|kb_config| {
            kb_config.bindings.contains(&keybinding_event.0)
          })
          .map(|kb_config| kb_config.commands.clone());

        if let Some(commands) = commands {
          self.process_commands(&commands, None, config)?;
        }

        return Ok(());
      }
      PlatformEvent::Mouse(event) => {
        handle_mouse_move(&event, state, config)
      }
      PlatformEvent::Window(window_event) => match window_event {
        WindowEvent::Focused { window, .. } => {
          handle_window_focused(&window, state, config)
        }
        WindowEvent::Shown { window, .. } => {
          handle_window_shown(window, state, config)
        }
        WindowEvent::Hidden { window, .. } => {
          handle_window_hidden(&window, state, config)
        }
        WindowEvent::MovedOrResized {
          window,
          is_interactive_start,
          is_interactive_end,
          ..
        } => handle_window_moved_or_resized(
          &window,
          is_interactive_start,
          is_interactive_end,
          state,
          config,
        ),
        WindowEvent::Minimized { window, .. } => {
          handle_window_minimized(&window, state, config)
        }
        WindowEvent::MinimizeEnded { window, .. } => {
          handle_window_minimize_ended(&window, state, config)
        }
        WindowEvent::TitleChanged { window, .. } => {
          handle_window_title_changed(&window, state, config)
        }
        WindowEvent::Destroyed { window_id, .. } => {
          handle_window_destroyed(window_id, state)
        }
      },
    }?;

    // 🔥 DEBOUNCED SYNC
    if !state.is_paused && state.pending_sync.has_changes() {
      const RETILE_DELAY: Duration = Duration::from_millis(200);

      if self.last_retile.elapsed() >= RETILE_DELAY {
        platform_sync(state, config)?;
        self.last_retile = Instant::now();
        self.retile_pending = false;
      } else {
        self.retile_pending = true;
      }
    }

    // 🔥 RUN DEFERRED SYNC
    if self.retile_pending {
      const RETILE_DELAY: Duration = Duration::from_millis(200);

      if self.last_retile.elapsed() >= RETILE_DELAY {
        platform_sync(state, config)?;
        self.last_retile = Instant::now();
        self.retile_pending = false;
      }
    }

    Ok(())
  }

  // ⛔ EVERYTHING BELOW UNCHANGED