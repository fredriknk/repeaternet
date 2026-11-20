use super::*;

impl Store {
    /// Save an immutable report for a complete, current run.
    pub fn save_report(
        &self,
        job_id: &str,
        report: &Value,
        current: &CurrentIdentity,
    ) -> Result<()> {
        let _lock = self.lock()?;
        let job = self.load_unlocked(job_id)?;
        let (run, id) = self.report_ids(report)?;
        if run != job_id
            || ["terrain_fingerprint", "radio_fingerprint", "model_version"]
                .iter()
                .any(|&key| report[key].is_null() || report[key] != job[key])
        {
            return Err(Error::Invalid("report identity does not match run"));
        }
        if job["state"] != "complete" || !current.matches(&job) || !self.complete_grid(&job)? {
            return Err(Error::Conflict);
        }
        let filename = format!("{run}-{id}.json");
        let path = self.path(Some("coverage-target-reports"), &filename, true)?;
        if path.try_exists()? {
            return Err(Error::Conflict);
        }
        let prefix = format!("{run}-");
        if self
            .entries("coverage-target-reports")?
            .iter()
            .filter(|entry| {
                entry.file_name().to_str().is_some_and(|name| {
                    name.strip_prefix(&prefix)
                        .and_then(|name| name.strip_suffix(".json"))
                        .is_some_and(|id| validate_id(id).is_ok())
                })
            })
            .count()
            >= MAX_REPORTS_PER_RUN
        {
            return Err(Error::ReportLimit);
        }
        write_json(&path, report, MAX_REPORT_BYTES)
    }

    pub fn load_report(&self, job_id: &str, report_id: &str) -> Result<Value> {
        let _lock = self.lock()?;
        self.read_report(job_id, report_id)
    }

    pub fn find_report(&self, report_id: &str) -> Result<Value> {
        validate_id(report_id)?;
        let _lock = self.lock()?;
        let mut matches = self
            .report_entries()?
            .into_iter()
            .filter(|(_, id)| id == report_id)
            .filter_map(|(run, id)| self.read_report(&run, &id).ok());
        let report = matches.next().ok_or(Error::NotFound)?;
        if matches.next().is_some() {
            return Err(Error::Conflict);
        }
        Ok(report)
    }

    /// List up to 100 valid reports ordered by assessment time.
    pub fn reports(&self) -> Result<Vec<Value>> {
        let _lock = self.lock()?;
        let mut reports: Vec<Value> = Vec::new();
        for (run, id) in self.report_entries()? {
            if let Ok(report) = self.read_report(&run, &id) {
                let time = assessment_time(&report);
                let index = reports.partition_point(|item| assessment_time(item) >= time);
                if index < 100 {
                    if reports.len() == 100 {
                        reports.pop();
                    }
                    reports.insert(index, report_summary(&report));
                }
            }
        }
        Ok(reports)
    }

    fn report_entries(&self) -> Result<Vec<(String, String)>> {
        let mut reports = Vec::new();
        for entry in self.entries("coverage-target-reports")? {
            let name = entry.file_name();
            if let Some((run, id)) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|name| name.split_once('-'))
            {
                if validate_id(run).is_ok() && validate_id(id).is_ok() {
                    reports.push((run.to_owned(), id.to_owned()));
                }
            }
        }
        Ok(reports)
    }

    fn report_ids<'a>(&self, report: &'a Value) -> Result<(&'a str, &'a str)> {
        if report["project_id"] != self.project_id {
            return Err(Error::NotFound);
        }
        let run = report["coverage_job_id"]
            .as_str()
            .ok_or(Error::Invalid("report run ID"))?;
        let id = report["report_id"]
            .as_str()
            .ok_or(Error::Invalid("report ID"))?;
        validate_id(run)?;
        validate_id(id)?;
        if !report["targets"]
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_object))
        {
            return Err(Error::Invalid("report targets"));
        }
        Ok((run, id))
    }

    fn read_report(&self, run: &str, id: &str) -> Result<Value> {
        validate_id(run)?;
        validate_id(id)?;
        let path = self.path(
            Some("coverage-target-reports"),
            &format!("{run}-{id}.json"),
            false,
        )?;
        let report = read_json(&path, MAX_REPORT_BYTES)?;
        if self.report_ids(&report)? != (run, id) {
            return Err(Error::NotFound);
        }
        Ok(report)
    }
}

fn assessment_time(report: &Value) -> f64 {
    report["assessed_at"].as_f64().unwrap_or_default()
}

fn report_summary(report: &Value) -> Value {
    let keys = [
        "report_id",
        "coverage_job_id",
        "assessed_at",
        "target_fingerprint",
        "terrain_fingerprint",
        "radio_fingerprint",
        "model_version",
        "grid_compatibility",
        "client_profile",
        "profile_step_m",
    ];
    let mut summary = Value::Object(
        keys.into_iter()
            .map(|key| (key.to_owned(), report[key].clone()))
            .collect(),
    );
    let mut counts = [0u64; 4];
    if let Some(targets) = report["targets"].as_array() {
        for target in targets {
            let index = match target["state"].as_str() {
                Some("pass") => 0,
                Some("fail") => 1,
                Some("unresolved") => 2,
                _ => 3,
            };
            counts[index] += 1;
        }
    }
    for (key, count) in [
        "pass_count",
        "fail_count",
        "unresolved_count",
        "unknown_count",
    ]
    .into_iter()
    .zip(counts)
    {
        summary[key] = json!(count);
    }
    summary["target_count"] = json!(counts.iter().sum::<u64>());
    summary["road_sample_spacing_m"] = report["assumptions"]["road_sample_spacing_m"].clone();
    summary
}
