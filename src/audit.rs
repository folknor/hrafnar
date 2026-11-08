use serde::Serialize;

#[derive(Serialize)]
struct AuditEntry {
    timestamp: String,
    project: String,
    archetype: String,
    provider: String,
    session: String,
    prompt: String,
    response: Option<String>,
    error: Option<String>,
    /// Flat run digest (codex): exit/signal/captured/turns/task_complete/usage.
    /// Absent for providers without a machine-readable stream (claude).
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<crate::provider::DigestSummary>,
    /// What the run's processes left behind (see `contained::Leftovers`);
    /// absent when nothing was.
    #[serde(skip_serializing_if = "Option::is_none")]
    leftovers: Option<crate::contained::Leftovers>,
}

pub(crate) fn log_result(
    data: &crate::data::DataDir,
    scope: &crate::data::LogScope,
    row: &crate::sessions::SessionRow,
) -> bool {
    let project_root = std::path::Path::new(&row.project);
    let dir = match data.audit(project_root, scope) {
        Some(d) => d,
        None => {
            eprintln!("warning: could not determine audit directory (HOME not set)");
            return false;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("warning: failed to create audit dir: {e}");
        return false;
    }

    let path = dir.join("audit.jsonl");

    let entry = AuditEntry {
        timestamp: row.timestamp.clone(),
        project: row.project.clone(),
        archetype: row.archetype.clone(),
        provider: row.provider.clone(),
        session: row.session_id.clone(),
        prompt: row.assembled_prompt.clone(),
        response: row.response.clone(),
        error: row.error.clone(),
        digest: row.digest.clone(),
        leftovers: row.leftovers.clone(),
    };

    let line = match serde_json::to_string(&entry) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("warning: failed to serialize audit entry: {e}");
            return false;
        }
    };

    match crate::fsutil::append_jsonl(&path, line.as_bytes()) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("warning: failed to append audit entry: {e}");
            false
        }
    }
}

pub(crate) fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    chrono_utc(secs)
}

/// Format a UNIX-seconds instant as a UTC `YYYY-MM-DDThh:mm:ssZ` string without
/// pulling in the chrono crate.
pub(crate) fn chrono_utc(secs: u64) -> String {
    // Convert to date-time components
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Days since epoch to Y-M-D (simplified, good enough for logging)
    let mut y = 1970i64;
    let mut remaining = days.cast_signed();
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    for md in &month_days {
        if remaining < *md as i64 {
            break;
        }
        remaining -= *md as i64;
        m += 1;
    }

    format!(
        "{y:04}-{:02}-{:02}T{hours:02}:{minutes:02}:{seconds:02}Z",
        m + 1,
        remaining + 1
    )
}
