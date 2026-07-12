//! Live database queries for the runtime domain.

use super::*;

/// One queue's message counts from the MysqlMq driver tables.
pub(crate) struct DbQueueCounts {
    pub queue: String,
    pub new: u32,
    pub in_progress: u32,
    pub retry: u32,
    pub error: u32,
    pub done: u32,
    pub oldest_waiting_secs: Option<i64>,
}

/// Backlog per db-connection queue: `queue_message_status` counts grouped by status
/// (constants from `MysqlMq\Model\QueueManagement`: 2 new, 3 in progress, 4 complete,
/// 5 retry, 6 error, 7 to-be-deleted) plus the oldest waiting (new/retry) message's age
/// on the DB server's clock. Queues with no messages still appear (from `queue`).
pub(crate) fn fetch_queue_backlog(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<DbQueueCounts>, String> {
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let names: Vec<String> =
        c.query(format!("SELECT name FROM {p}queue")).map_err(clean_err)?;
    let mut by_name: HashMap<String, DbQueueCounts> = names
        .into_iter()
        .map(|queue| {
            (
                queue.clone(),
                DbQueueCounts {
                    queue,
                    new: 0,
                    in_progress: 0,
                    retry: 0,
                    error: 0,
                    done: 0,
                    oldest_waiting_secs: None,
                },
            )
        })
        .collect();

    let counts: Vec<(String, u8, u64)> = c
        .query(format!(
            "SELECT q.name, s.status, COUNT(*) FROM {p}queue_message_status s \
             JOIN {p}queue q ON q.id = s.queue_id GROUP BY q.name, s.status"
        ))
        .map_err(clean_err)?;
    for (name, status, n) in counts {
        let Some(e) = by_name.get_mut(&name) else { continue };
        let n = n as u32;
        match status {
            2 => e.new = n,
            3 => e.in_progress = n,
            5 => e.retry = n,
            6 => e.error = n,
            4 | 7 => e.done += n,
            _ => {}
        }
    }

    let oldest: Vec<(String, Option<i64>)> = c
        .query(format!(
            "SELECT q.name, TIMESTAMPDIFF(SECOND, MIN(s.updated_at), NOW()) \
             FROM {p}queue_message_status s JOIN {p}queue q ON q.id = s.queue_id \
             WHERE s.status IN (2, 5) GROUP BY q.name"
        ))
        .map_err(clean_err)?;
    for (name, secs) in oldest {
        if let Some(e) = by_name.get_mut(&name) {
            e.oldest_waiting_secs = secs;
        }
    }

    Ok(by_name.into_values().collect())
}

/// One job's aggregated `cron_schedule` stats.
pub(crate) struct DbCronStat {
    pub job_code: String,
    pub pending: u32,
    pub running: u32,
    pub success: u32,
    pub error: u32,
    pub missed: u32,
    pub last_status: Option<String>,
    pub last_run: Option<String>,
    pub last_run_secs: Option<i64>,
    pub last_duration_secs: Option<i64>,
    pub last_error: Option<String>,
    pub next_scheduled: Option<String>,
}

/// Per-job `cron_schedule` summary: status counts, the most recently *started* run (its
/// status is the job's last outcome; duration = finished − executed), the most recent
/// retained error message, and the next pending run. All ages on the DB server's clock.
pub(crate) fn fetch_cron_stats(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<DbCronStat>, String> {
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let mut by_code: HashMap<String, DbCronStat> = HashMap::new();
    fn stat<'a>(m: &'a mut HashMap<String, DbCronStat>, code: &str) -> &'a mut DbCronStat {
        m.entry(code.to_string()).or_insert_with(|| DbCronStat {
            job_code: code.to_string(),
            pending: 0,
            running: 0,
            success: 0,
            error: 0,
            missed: 0,
            last_status: None,
            last_run: None,
            last_run_secs: None,
            last_duration_secs: None,
            last_error: None,
            next_scheduled: None,
        })
    }

    let counts: Vec<(String, String, u64)> = c
        .query(format!(
            "SELECT job_code, status, COUNT(*) FROM {p}cron_schedule \
             GROUP BY job_code, status"
        ))
        .map_err(clean_err)?;
    for (code, status, n) in counts {
        let s = stat(&mut by_code, &code);
        let n = n as u32;
        match status.as_str() {
            "pending" => s.pending = n,
            "running" => s.running = n,
            "success" => s.success = n,
            "error" => s.error = n,
            "missed" => s.missed = n,
            _ => {}
        }
    }

    // The most recently started row per job = the last outcome.
    let last: Vec<(String, String, Option<String>, Option<i64>, Option<i64>)> = c
        .query(format!(
            "SELECT s.job_code, s.status, CAST(s.executed_at AS CHAR), \
             TIMESTAMPDIFF(SECOND, s.executed_at, NOW()), \
             TIMESTAMPDIFF(SECOND, s.executed_at, s.finished_at) \
             FROM {p}cron_schedule s \
             JOIN (SELECT job_code, MAX(executed_at) me FROM {p}cron_schedule \
                   WHERE executed_at IS NOT NULL GROUP BY job_code) m \
             ON m.job_code = s.job_code AND s.executed_at = m.me"
        ))
        .map_err(clean_err)?;
    for (code, status, run, secs, duration) in last {
        let s = stat(&mut by_code, &code);
        if s.last_status.is_none() {
            s.last_status = Some(status);
            s.last_run = run;
            s.last_run_secs = secs;
            s.last_duration_secs = duration;
        }
    }

    let errors: Vec<(String, Option<String>)> = c
        .query(format!(
            "SELECT s.job_code, s.messages FROM {p}cron_schedule s \
             JOIN (SELECT job_code, MAX(schedule_id) mi FROM {p}cron_schedule \
                   WHERE status = 'error' GROUP BY job_code) m \
             ON m.mi = s.schedule_id"
        ))
        .map_err(clean_err)?;
    for (code, msg) in errors {
        stat(&mut by_code, &code).last_error = msg.filter(|m| !m.is_empty());
    }

    let next: Vec<(String, Option<String>)> = c
        .query(format!(
            "SELECT job_code, CAST(MIN(scheduled_at) AS CHAR) FROM {p}cron_schedule \
             WHERE status = 'pending' GROUP BY job_code"
        ))
        .map_err(clean_err)?;
    for (code, at) in next {
        stat(&mut by_code, &code).next_scheduled = at;
    }

    Ok(by_code.into_values().collect())
}

/// A job's recent history rows — runs, errors, and misses (pending rows are excluded:
/// Magento schedules ahead, so dozens of future pendings would drown the log), newest
/// first. `(status, scheduled_at, executed_at, finished_at, duration, messages)`.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_cron_history(
    conn: &DbConnection,
    table_prefix: &str,
    job_code: &str,
    limit: usize,
) -> Result<
    Vec<(String, Option<String>, Option<String>, Option<String>, Option<i64>, Option<String>)>,
    String,
> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    c.exec(
        format!(
            "SELECT status, CAST(scheduled_at AS CHAR), CAST(executed_at AS CHAR), \
             CAST(finished_at AS CHAR), TIMESTAMPDIFF(SECOND, executed_at, finished_at), \
             messages FROM {table_prefix}cron_schedule \
             WHERE job_code = :code AND status <> 'pending' \
             ORDER BY COALESCE(executed_at, scheduled_at) DESC, schedule_id DESC \
             LIMIT {limit}"
        ),
        params! { "code" => job_code },
    )
    .map_err(clean_err)
}

/// Seconds since the last *successful* cron job finished, per the DB server's own clock
/// (`TIMESTAMPDIFF` — no client-side time needed). `None` = no successful runs recorded.
pub(crate) fn fetch_cron_last_success(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Option<i64>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    c.query_first(format!(
        "SELECT TIMESTAMPDIFF(SECOND, MAX(finished_at), NOW()) FROM {table_prefix}cron_schedule \
         WHERE status = 'success'"
    ))
    .map_err(clean_err)
    .map(Option::flatten)
}

/// Count the store hierarchy — `(websites, store groups, store views)` — excluding the
/// synthetic admin scopes (id 0).
pub(crate) fn fetch_scope_counts(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<(usize, usize, usize), String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let count = |c: &mut mysql::Conn, sql: String| -> Result<usize, String> {
        Ok(c.query_first::<u64, _>(sql).map_err(clean_err)?.unwrap_or(0) as usize)
    };
    let websites = count(&mut c, format!("SELECT COUNT(*) FROM {p}store_website WHERE website_id > 0"))?;
    let groups = count(&mut c, format!("SELECT COUNT(*) FROM {p}store_group WHERE group_id > 0"))?;
    let stores = count(&mut c, format!("SELECT COUNT(*) FROM {p}store WHERE store_id > 0"))?;
    Ok((websites, groups, stores))
}
