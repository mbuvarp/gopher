use super::{Github, PrRef, ResponsePolicy};
use crate::actions::{Label, MergeMethod};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) fn message(response: &Value, fallback: &str) -> String {
    response["message"]
        .as_str()
        .unwrap_or(fallback)
        .chars()
        .filter(|c| !c.is_control())
        .take(400)
        .collect()
}

impl Github {
    async fn rest(&self, endpoint: &str) -> Result<Value> {
        self.execute(&["api", "--hostname", "github.com", endpoint], None)
            .await
    }
    async fn mutation(
        &self,
        endpoint: &str,
        method: &str,
        payload: Option<&Value>,
    ) -> Result<Value> {
        let mut args = vec![
            "api",
            "--hostname",
            "github.com",
            "--method",
            method,
            endpoint,
        ];
        if payload.is_some() {
            args.extend(["--input", "-"]);
        }
        self.execute_response(&args, payload, ResponsePolicy::Mutation)
            .await
    }

    pub async fn merge_pr(&self, pr: &PrRef, head: &str, method: MergeMethod) -> Result<()> {
        ensure!(!head.is_empty(), "Cannot merge without a known commit");
        let response = self
            .mutation(
                &format!("repos/{}/pulls/{}/merge", pr.repo, pr.number),
                "PUT",
                Some(&json!({"sha":head, "merge_method":method.api_name()})),
            )
            .await?;
        ensure!(
            response["merged"].as_bool() == Some(true),
            "GitHub: {}",
            message(&response, "Merge was blocked")
        );
        Ok(())
    }

    async fn label_pages(&self, endpoint: &str) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        for page in 1.. {
            let response = self
                .rest(&format!("{endpoint}?per_page=100&page={page}"))
                .await?;
            let labels = response
                .as_array()
                .context("GitHub returned invalid labels")?;
            result.extend(labels.iter().cloned());
            if labels.len() < 100 {
                return Ok(result);
            }
        }
        unreachable!()
    }

    pub async fn labels(&self, pr: &PrRef) -> Result<Vec<Label>> {
        let available = self
            .label_pages(&format!("repos/{}/labels", pr.repo))
            .await?;
        let assigned = self
            .label_pages(&format!("repos/{}/issues/{}/labels", pr.repo, pr.number))
            .await?;
        let selected = assigned
            .iter()
            .filter_map(|l| l["name"].as_str())
            .collect::<BTreeSet<_>>();
        let mut labels = available
            .iter()
            .map(|label| {
                let name = label["name"].as_str().context("Missing label name")?;
                Ok(Label {
                    name: name.into(),
                    color: label["color"].as_str().unwrap_or("808080").into(),
                    selected: selected.contains(name),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        labels.sort_by_cached_key(|l| l.name.to_lowercase());
        Ok(labels)
    }

    pub async fn set_label(&self, pr: &PrRef, name: &str, selected: bool) -> Result<()> {
        let endpoint = format!("repos/{}/issues/{}/labels", pr.repo, pr.number);
        if selected {
            self.mutation(&endpoint, "POST", Some(&json!({"labels":[name]})))
                .await?;
        } else {
            // A label name can contain slashes, spaces, commas, or Unicode.
            let mut url = url::Url::parse(&format!("https://github.com/{endpoint}"))?;
            url.path_segments_mut()
                .map_err(|_| anyhow::anyhow!("Invalid label URL"))?
                .push(name);
            self.mutation(url.path().trim_start_matches('/'), "DELETE", None)
                .await?;
        }
        Ok(())
    }
}
