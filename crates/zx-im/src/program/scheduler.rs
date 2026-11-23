//! The scheduler: each schedule of im.toml asks the server for its conversation at a fixed
//! interval.

use std::time::Duration;

use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::{Trigger, Triggers};

/// Ask `triggers` for each trigger of `schedules` every time its interval passes, the first
/// time one interval after the start, until `cancel` or until the server no longer runs. An
/// interval that passes while the server keeps the last one waiting is skipped, not made up.
pub(super) async fn run(schedules: Vec<(Duration, Trigger)>, triggers: Triggers, cancel: CancellationToken) {
    let schedules = schedules
        .into_iter()
        .map(|(every, trigger)| tick(every, trigger, triggers.clone(), cancel.clone()));
    futures::future::join_all(schedules).await;
}

/// One schedule: `trigger`, asked for every `every`.
async fn tick(every: Duration, trigger: Trigger, triggers: Triggers, cancel: CancellationToken) {
    let mut interval = tokio::time::interval_at(Instant::now() + every, every);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = interval.tick() => {
                if !triggers.start(trigger.clone()).await {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConversationId;

    /// Long enough for a test that does not fail, reached only when it does.
    const WAIT: Duration = Duration::from_secs(10);

    fn trigger(name: &str) -> Trigger {
        Trigger {
            platform: "slack".into(),
            chat: ConversationId("C1".into()),
            title: name.into(),
            prompt: "look around".into(),
            source: format!("schedule:{name}"),
        }
    }

    #[tokio::test]
    async fn each_schedule_asks_for_its_trigger_every_interval_until_cancelled() {
        let (triggers, mut asked) = Triggers::channel();
        let cancel = CancellationToken::new();
        let schedules = vec![
            (Duration::from_millis(20), trigger("often")),
            (Duration::from_secs(3600), trigger("hourly")),
        ];
        let running = tokio::spawn(run(schedules, triggers, cancel.clone()));
        for _ in 0..2 {
            let next = tokio::time::timeout(WAIT, asked.recv()).await.unwrap();
            assert_eq!(next, Some(trigger("often")));
        }
        cancel.cancel();
        tokio::time::timeout(WAIT, running).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn schedules_end_once_the_server_no_longer_takes_triggers() {
        let (triggers, asked) = Triggers::channel();
        drop(asked);
        let schedules = vec![(Duration::from_millis(10), trigger("often"))];
        let running = tokio::spawn(run(schedules, triggers, CancellationToken::new()));
        tokio::time::timeout(WAIT, running).await.unwrap().unwrap();
    }
}
