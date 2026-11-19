//! The app around the pages: window, splash, menus, history, release notes, general switches.

use crate::*;

impl Lightforge {
    pub(crate) fn update_app(&mut self, message: Message) -> Result<Task<Message>, Message> {
        self.update_window(message)
            .or_else(|m| self.update_preferences(m))
            .or_else(|m| self.update_pages(m))
    }

    /// The window: closing, sizing, focus, and the splash in front of it.
    fn update_window(&mut self, message: Message) -> Result<Task<Message>, Message> {
        Ok(match message {
            Message::ResizeTraced(size) => {
                trace::mark(format_args!("resize {}x{}", size.width, size.height));
                Task::none()
            }

            Message::CloseRequested(id) => {
                if self.config.close_to_tray && self.closing.is_none() {
                    if let Some(hide) = self.hide_to_tray() {
                        return Ok(hide);
                    }
                }
                self.quit_window(id)
            }

            // A second one does not wait for the checkpoint.
            Message::StopSignal if self.closing.is_some() => iced::exit(),
            Message::StopSignal => self.quit_window(self.window_id),

            Message::SplashDone(count) => {
                // The wizard counts off this same fetch.
                if let Some(wizard) = self.wizard.as_mut() {
                    wizard.prefetched = Some(count);
                }
                match self.splash.is_some() {
                    true => self.close_splash_after_floor(),
                    false => Task::none(),
                }
            }

            Message::FinishSplash => self.finish_splash(),

            Message::Exit => self.quit_window(self.window_id),

            Message::Tray(tray::Event::Available(available)) => {
                self.tray_available = available;
                match !available && self.tray_hidden {
                    true => self.restore_from_tray(),
                    false => Task::none(),
                }
            }
            Message::Tray(tray::Event::Show) => self.restore_from_tray(),
            Message::Tray(tray::Event::Quit) => Task::done(Message::Exit),
            Message::Tray(tray::Event::Play(code)) => self.launch_from_tray(code),
            Message::TrayStartupTimeout => match self.tray_hidden && !self.tray_available {
                true => self.restore_from_tray(),
                false => Task::none(),
            },

            // Sizes come in points at iced's scale then; across a scale change
            // they are stale (measured: 1080 px read as 1458). Tagged by
            // generation, a stale reading is asked again.
            Message::WindowResized => {
                let generation = self.ui_scale_gen;
                Task::perform(
                    async { tokio::time::sleep(std::time::Duration::from_millis(50)).await },
                    |()| (),
                )
                .then(|()| iced::window::oldest())
                .and_then(iced::window::size)
                .map(move |size| Message::WindowHeight(generation, size.height))
            }

            Message::WindowHeight(generation, _) if generation != self.ui_scale_gen => {
                Task::done(Message::WindowResized)
            }

            Message::WindowHeight(_, points) => {
                let scale = fit::scale_for(points * self.ui_scale);
                if scale != self.ui_scale {
                    crate::trace::mark(format!("gamescope: ui scale {} -> {scale}", self.ui_scale));
                    self.ui_scale = scale;
                    self.ui_scale_gen += 1;
                }
                Task::none()
            }

            // The monitor comes in points at the scale in effect, so it is
            // multiplied back before the new scale is worked out.
            Message::WindowFit { id, monitor, position } => {
                if self.console() {
                    return Ok(Task::none());
                }
                let min_size = iced::window::set_min_size(id, Some(WINDOW_MIN));
                if fit::owned() {
                    return Ok(min_size);
                }
                let chosen = f32::from(self.config.interface_size) / 100.0;
                let was = self.ui_scale;
                self.ui_scale = match monitor {
                    Some(monitor) => fit::desktop_scale(chosen, monitor * was),
                    None => chosen,
                };
                // Sizes in these tasks are read at the new scale: iced applies
                // it before it runs them.
                let resized = min_size.chain(iced::window::resize(id, WINDOW));
                // Only X11 says where a window is; there it grows about its middle.
                match position.filter(|_| self.ui_scale != was) {
                    Some(at) => {
                        let grown = WINDOW * ((self.ui_scale - was) / 2.0);
                        let at = iced::Point::new(at.x - grown.width, at.y - grown.height);
                        resized.chain(iced::window::move_to(id, at))
                    }
                    None => resized,
                }
            }

            Message::WindowFocused(focused) => {
                self.window_focused = focused && !self.tray_hidden;
                Task::none()
            }

            // Compositors may refuse the focus (Wayland's focus-stealing
            // rules); the attention request still marks the taskbar entry.
            Message::Raise => self.restore_from_tray(),
            Message::PlayRequested(code) => {
                self.play_request = Some((code, Instant::now()));
                Task::none()
            }
            other => return Err(other),
        })
    }

    fn quit_window(&mut self, id: iced::window::Id) -> Task<Message> {
        if self.env_unsaved {
            self.save_config();
        }
        if let Some(cancel) = &self.deep_cancel {
            cancel.cancel();
        }
        // Preserve the running queue entry until its writer checkpoints.
        if let Some(code) = self.running().map(|d| d.code.clone()) {
            self.queue_frozen = true;
            self.pause(&code);
        }
        self.closing = Some(id);
        if let Some(warming) = self.warming.take() {
            warming.cancel();
        }
        match self.store_writer() {
            Some(what) => {
                self.status = format!("finishing {what} safely…");
                Task::none()
            }
            None => iced::exit(),
        }
    }

    /// App-wide switches, the release notes, and Lightforge's own updates.
    fn update_preferences(&mut self, message: Message) -> Result<Task<Message>, Message> {
        Ok(match message {
            Message::ToggleHints => {
                self.hints = !self.hints;
                Task::none()
            }

            Message::MutePatchNotes(mute) => {
                if let Some(notes) = self.patch_notes.as_mut() {
                    notes.mute = mute;
                }
                Task::none()
            }

            Message::ShowReleaseNotes(show) => {
                self.config.patch_notes_muted = !show;
                self.save_config();
                Task::none()
            }

            Message::DismissPatchNotes => {
                if let Some(notes) = self.patch_notes.take() {
                    // Recorded on the way out, so a start that was killed
                    // before anyone read them shows them again.
                    self.config.patch_notes_seen = Some(notes.release.version.clone());
                    self.config.patch_notes_muted = notes.mute;
                    self.save_config();
                }
                Task::none()
            }

            Message::CheckLauncherUpdates(on) => {
                self.config.check_for_updates = on;
                self.save_config();
                Task::none()
            }

            Message::AddToSteam => {
                self.add_to_steam();
                Task::none()
            }

            Message::ControllerLook(on) => {
                self.config.steamos_use = Some(match on {
                    true => SteamOsUse::Console,
                    false => SteamOsUse::Desktop,
                });
                self.save_config();
                self.window_mode()
            }

            Message::InterfaceSize(InterfaceSizeChoice(percent)) => {
                self.config.interface_size = percent;
                self.save_config();
                self.fit_window()
            }

            Message::MinimizeOnLaunch(on) => {
                self.config.minimize_on_launch = on;
                self.save_config();
                Task::none()
            }

            Message::CloseToTray(on) => {
                self.config.close_to_tray = on;
                self.save_config();
                Task::none()
            }

            Message::HideToTrayOnLaunch(on) => {
                self.config.hide_to_tray_on_launch = on;
                self.save_config();
                Task::none()
            }

            Message::CloseBattleNetOnPlay(on) => {
                self.config.close_battle_net_on_play = on;
                self.save_config();
                Task::none()
            }

            Message::BattleNetMinimized(on) => {
                self.config.battle_net_minimized = on;
                self.save_config();
                Task::none()
            }

            Message::BattleNetInBackground(on) => {
                self.config.battle_net_in_background = on;
                self.save_config();
                // Under Game Mode a restart only closes the app.
                if fit::under_gamescope() {
                    return Ok(self.apply_mode());
                }
                self.mode_restart = Some(on);
                Task::none()
            }

            Message::ModeRestart => {
                self.mode_restart = None;
                // The exit stops a client only while the setting is on, and
                // this one has just been switched off.
                match self.config.battle_net_in_background {
                    true => Task::done(Message::UpgradeRestart),
                    false => Task::perform(
                        tasks::close_background_client(self.config.clone(), self.layout.clone()),
                        |_| Message::UpgradeRestart,
                    ),
                }
            }

            Message::ModeRestartLater => {
                self.mode_restart = None;
                self.apply_mode()
            }
            other => return Err(other),
        })
    }

    /// Moving between pages, links, and the menus and pickers that open over them.
    fn update_pages(&mut self, message: Message) -> Result<Task<Message>, Message> {
        Ok(match message {
            Message::Show(screen) => {
                self.go_to(screen);
                // Including when the page did not change: the menu entry was
                // still a click, and it has had its answer.
                self.close_panels();
                self.error = None;
                self.arrived()
            }

            Message::GoBack => {
                if let Some(screen) = self.visited.pop() {
                    self.forward.push(self.screen);
                    self.screen = screen;
                    self.error = None;
                }
                self.arrived()
            }

            Message::GoForward => {
                if let Some(screen) = self.forward.pop() {
                    self.visited.push(self.screen);
                    self.screen = screen;
                    self.error = None;
                }
                self.arrived()
            }

            Message::ToggleAppMenu => {
                self.app_menu_open = !self.app_menu_open;
                Task::none()
            }

            Message::OpenLink(url) => {
                if let Err(e) = tasks::open_url(url) {
                    self.error = Some(e.into());
                }
                self.close_panels();
                Task::none()
            }

            // From addon pages and descriptions: `xdg-open` would hand a
            // `file://` or `smb://` link to whatever handles it.
            Message::OpenUrl(url) => {
                let web = ["https://", "http://"]
                    .iter()
                    .any(|scheme| url.get(..scheme.len()).is_some_and(|start| start.eq_ignore_ascii_case(scheme)));
                let opened = match web {
                    true => tasks::open_url(&url),
                    false => Err(format!("not opening {url}: only web links are opened")),
                };
                if let Err(e) = opened {
                    self.error = Some(e.into());
                }
                Task::none()
            }

            Message::TogglePicker(key) => {
                self.picker_open = match self.picker_open.as_ref() == Some(&key) {
                    true => None,
                    false => Some(key),
                };
                Task::none()
            }

            Message::Picked(message) => {
                self.picker_open = None;
                self.update(*message)
            }

            Message::ToggleGear => {
                self.gear_open = !self.gear_open;
                Task::none()
            }

            Message::Dismiss => {
                self.close_panels();
                Task::none()
            }

            Message::MenuClick => Task::none(),
            other => return Err(other),
        })
    }
}
