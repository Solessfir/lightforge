use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::paths::Layout;
use crate::product::ProductCode;

use super::readiness::{line_time, session_lines, sign_in_state};

const DAY: Duration = Duration::from_secs(86_400);

/// The selected WoW license's game time, reported by the signed-in client.
#[derive(Clone, Copy, Debug)]
pub struct GameTime {
    pub expires_at: SystemTime,
    pub observed_at: SystemTime,
}

impl GameTime {
    pub fn days_remaining(self, now: SystemTime) -> u64 {
        self.expires_at.duration_since(now).map_or(0, |left| {
            left.as_secs() / DAY.as_secs()
                + u64::from(left.as_secs() % DAY.as_secs() != 0 || left.subsec_nanos() != 0)
        })
    }
}

/// Reads only the latest main session. Missing or uncertain account data stays unknown.
pub fn game_time(
    layout: &Layout,
    user: &str,
    code: &ProductCode,
    now: SystemTime,
) -> Option<GameTime> {
    let mut lines = session_lines(layout, user)?;
    let time = lines_time(lines.by_ref(), code, now);
    if lines.failed { None } else { time }
}

#[cfg(test)]
fn session_time(text: &str, code: &ProductCode, now: SystemTime) -> Option<GameTime> {
    lines_time(text.lines().map(|line| Some(line.to_owned())), code, now)
}

/// `None` lines are skipped records, which may have hidden a sign-out.
fn lines_time(lines: impl Iterator<Item = Option<String>>, code: &ProductCode, now: SystemTime) -> Option<GameTime> {
    let mut signed_in = false;
    // The client logs one selection per WoW product it shows, all in one group.
    let mut selected = HashMap::new();
    let mut times = HashMap::new();
    for line in lines {
        let state = match &line {
            Some(line) => sign_in_state(line),
            None => Some(false),
        };
        if let Some(state) = state {
            signed_in = state;
            if !state {
                selected.clear();
                times.clear();
            }
        }
        let Some(line) = line else { continue };
        if let Some(account) = line.split_once("New selection for product group WoW: account=") {
            match selection(account.1) {
                Some((product, region, license)) => {
                    selected.insert(product.to_owned(), (region.to_owned(), license));
                }
                None => selected.clear(),
            }
        }
        if let Some((_, info)) = line.split_once("Received GameTimeInfo: (Game: ") {
            let (game, info) = info.split_once(") info=")?;
            let Some((region, license)) = game.split_once("-WoW-") else {
                continue;
            };
            let license = license.parse::<u64>().ok()?;
            let expiry = info
                .split('|')
                .find_map(|field| field.strip_prefix("play_time_expires="));
            let time = expiry.and_then(|expiry| {
                let expires_at =
                    UNIX_EPOCH.checked_add(Duration::from_micros(expiry.parse().ok()?))?;
                Some(GameTime {
                    expires_at,
                    observed_at: timestamp(&line)?,
                })
            });
            times.insert((region.to_owned(), license), time);
        }
    }
    if !signed_in {
        return None;
    }
    // Retail is `retail` in the selection, not its product code `wow`.
    let account = (*code == ProductCode::Retail)
        .then(|| selected.get("retail"))
        .flatten()
        .or_else(|| selected.get(code.as_str()))?;
    let time = times.get(account).copied().flatten()?;
    (now.duration_since(time.observed_at).ok()? <= DAY && time.expires_at > now).then_some(time)
}

fn selection(account: &str) -> Option<(&str, &str, u64)> {
    let account = account.strip_prefix("GameAccount(WoW:")?;
    let (product, account) = account.split_once(" region=")?;
    let (region, account) = account.split_once(" entity=")?;
    let entity = account.split(' ').next()?;
    let mut parts = entity.split(':');
    let (kind, realm, license) = (parts.next()?, parts.next()?, parts.next()?);
    u64::from_str_radix(kind, 16).ok()?;
    u64::from_str_radix(realm, 16).ok()?;
    if parts.next().is_some() || product.is_empty() || region.is_empty() {
        return None;
    }
    Some((product, region, u64::from_str_radix(license, 16).ok()?))
}

fn timestamp(line: &str) -> Option<SystemTime> {
    let date = line.get(2..12)?;
    let mut parts = date.split('-');
    let year = parts.next()?.parse::<u32>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 => 28 + u32::from(leap),
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    let hours = line.get(13..15)?.parse::<u32>().ok()?;
    let minutes = line.get(16..18)?.parse::<u32>().ok()?;
    let seconds = line.get(19..)?.split(' ').next()?.parse::<f64>().ok()?;
    if year < 1970
        || day == 0
        || day > days
        || hours > 23
        || minutes > 59
        || !(0.0..60.0).contains(&seconds)
    {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::try_from_secs_f64(line_time(line)?).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGN_IN: &str =
        "I 2026-11-17 00:00:02.000000 [BNLogin] Logged into Battle.net successfully.\n";
    const SELECT: &str = "D 2026-11-17 00:00:01.000000 [GameController] New selection for product group WoW: account=GameAccount(WoW:wowf region=EU entity=1:2:2a name=WoW1)\n";

    fn now() -> SystemTime {
        timestamp("I 2026-11-17 00:00:03.000000 [Main]").unwrap()
    }

    fn info(region: &str, license: u64, expiry: Option<SystemTime>) -> String {
        let value = expiry.map_or(String::new(), |expiry| {
            format!(
                "|play_time_expires={}|is_subscription=1",
                expiry.duration_since(UNIX_EPOCH).unwrap().as_micros()
            )
        });
        format!(
            "D 2026-11-16 23:59:59.000000 [GSAccountProvider] Received GameTimeInfo: (Game: {region}-WoW-{license}) info={value}\n"
        )
    }

    fn log() -> String {
        info("EU", 42, Some(now() + DAY)) + SELECT + SIGN_IN
    }

    #[test]
    fn matches_selected_license_and_rounds_up() {
        let time = session_time(&log(), &ProductCode::Forever, now()).unwrap();
        assert_eq!(time.days_remaining(now()), 1);
        assert_eq!(time.days_remaining(now() - Duration::from_nanos(1)), 2);
        assert_eq!(time.days_remaining(time.expires_at), 0);
        assert!(session_time(&log(), &ProductCode::Retail, now()).is_none());
        assert!(
            session_time(
                &log().replace("WoW:wowf", "WoW:retail"),
                &ProductCode::Retail,
                now()
            )
            .is_some()
        );
        assert!(
            session_time(
                &log().replace("entity=1:2:2a", "entity=1:2:2b"),
                &ProductCode::Forever,
                now()
            )
            .is_none()
        );
        assert!(
            session_time(
                &log().replace("region=EU", "region=US"),
                &ProductCode::Forever,
                now()
            )
            .is_none()
        );
    }

    #[test]
    fn each_product_keeps_its_own_selection() {
        let both = log().replace(SELECT, &(SELECT.replace("WoW:wowf", "WoW:retail") + SELECT));
        assert!(session_time(&both, &ProductCode::Retail, now()).is_some(), "a later wowf selection must not hide retail");
        assert!(session_time(&both, &ProductCode::Forever, now()).is_some());
        assert!(session_time(&both, &ProductCode::Classic, now()).is_none());
    }

    #[test]
    fn empty_malformed_expired_or_changed_accounts_stay_unknown() {
        for extra in [
            info("EU", 42, None),
            info("EU", 42, Some(now())),
            SELECT.replace("entity=1:2:2a", "entity=1:2:2b"),
            SELECT.replace("GameAccount(WoW:", "None(WoW:"),
        ] {
            assert!(session_time(&(log() + &extra), &ProductCode::Forever, now()).is_none());
        }
        assert!(
            session_time(
                &log().replace("play_time_expires=", "play_time_expires=bad"),
                &ProductCode::Forever,
                now()
            )
            .is_none()
        );
        assert!(
            session_time(
                &(log() + "I 2026-11-17 00:01:00.000000 [BNLogin] Logged out\n"),
                &ProductCode::Forever,
                now()
            )
            .is_none()
        );
        assert!(session_time(&log().replace(SIGN_IN, ""), &ProductCode::Forever, now()).is_none());
    }

    #[test]
    fn full_date_handles_midnight_and_rejects_stale_or_invalid_times() {
        assert!(session_time(&log(), &ProductCode::Forever, now()).is_some());
        assert!(session_time(&log(), &ProductCode::Forever, now() + DAY).is_none());
        assert!(session_time(&log(), &ProductCode::Forever, now() - DAY).is_none());
        for date in [
            "2026-02-30 12:00:00.0",
            "2026-13-01 12:00:00.0",
            "2026-11-17 24:00:00.0",
            "2026-11-17 12:00:NaN",
        ] {
            assert!(timestamp(&format!("I {date} [Main]")).is_none());
        }
    }

    #[test]
    fn skips_only_explicit_ipc_sessions_and_never_an_unsigned_main_session() {
        let root = tempfile::tempdir().unwrap();
        let layout = Layout::with_root(root.path());
        let dir = layout.battle_net_logs("test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("battle.net-20261117T000000.000000.log"), log()).unwrap();
        let newer = dir.join("battle.net-20261117T000100.000000.log");
        std::fs::write(
            &newer,
            "Leaving because another instance of battle.net is running",
        )
        .unwrap();
        assert!(game_time(&layout, "test", &ProductCode::Forever, now()).is_some());
        std::fs::write(&newer, "Logging started for Battle.net").unwrap();
        assert!(game_time(&layout, "test", &ProductCode::Forever, now()).is_none());
        std::fs::write(&newer, "x".repeat(64 * 1024 + 1)).unwrap();
        assert!(game_time(&layout, "test", &ProductCode::Forever, now()).is_none());
        std::fs::remove_file(&newer).unwrap();
        let long_session = log() + &format!("{}\n", "x".repeat(1023)).repeat(5 * 1024);
        std::fs::write(dir.join("battle.net-20261117T000000.000000.log"), long_session).unwrap();
        assert!(game_time(&layout, "test", &ProductCode::Forever, now()).is_some(), "a long session keeps its game time");
    }
}
