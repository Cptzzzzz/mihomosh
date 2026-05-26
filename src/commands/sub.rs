use std::{fs, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::ClientBuilder;
use serde_yml::{Mapping, Value};
use url::Url;

use crate::{models::config::Config, println_success};

pub(crate) const SUBSCRIPTION_SECTIONS: [&str; 3] = ["proxies", "proxy-groups", "rules"];

pub async fn handle_sub(url: Url) -> Result<()> {
    let cfg = Config::get_instance();

    let current = fs::read_to_string(&cfg.mihomo_path)
        .with_context(|| format!("Fail to read Mihomo config `{}`", cfg.mihomo_path.display()))?;
    let current_value = serde_yml::from_str::<Value>(&current).with_context(|| {
        format!(
            "Fail to parse current Mihomo config `{}`",
            cfg.mihomo_path.display()
        )
    })?;
    if !current_value.is_mapping() {
        bail!("Current Mihomo config must be a YAML object");
    }

    let subscription = fetch_meta_subscription_config(url)
        .await
        .context("Fail to fetch subscription")?;

    let data = replace_subscription_sections(&current, &subscription)
        .context("Fail to merge subscription into current Mihomo config")?;
    serde_yml::from_str::<Value>(&data).context("Merged Mihomo config is not valid YAML")?;

    fs::write(&cfg.mihomo_path, data).with_context(|| {
        format!(
            "Fail to write current Mihomo config `{}`",
            cfg.mihomo_path.display()
        )
    })?;

    cfg.get_api()
        .restart()
        .await
        .context("Fail to restart Mihomo")?;

    println_success!("Subscription applied to Mihomo");
    Ok(())
}

pub(crate) async fn fetch_meta_subscription_config(url: Url) -> Result<Value> {
    let data = fetch_meta_subscription(url).await?;
    serde_yml::from_str::<Value>(&data).context("Fail to parse subscription as Mihomo config")
}

async fn fetch_meta_subscription(url: Url) -> Result<String> {
    let url = make_meta_url(url);
    ClientBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent(format!(
            "mihomosh/v{} (clash-verge)",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .context("Fail to build reqwest client")?
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("Fail to send `GET {url}`"))?
        .error_for_status()
        .with_context(|| format!("Fail to request `GET {url}`"))?
        .text()
        .await
        .with_context(|| format!("Fail to read response from `GET {url}`"))
}

fn make_meta_url(mut url: Url) -> Url {
    let pairs = url
        .query_pairs()
        .filter(|(key, _)| key != "flag")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();

    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in pairs {
            query.append_pair(&key, &value);
        }
        query.append_pair("flag", "meta");
    }

    url
}

pub(crate) fn replace_subscription_sections(current: &str, subscription: &Value) -> Result<String> {
    let subscription = subscription
        .as_mapping()
        .ok_or_else(|| anyhow!("Subscription config must be a YAML object"))?;

    let mut replacements = Vec::new();
    let mut appends = Vec::new();
    for section in SUBSCRIPTION_SECTIONS {
        let value = get_subscription_section(subscription, section)?;
        let content = render_section(section, value)?;
        if let Some((start, end)) = find_top_level_section_range(current, section) {
            replacements.push((start, end, content));
        } else {
            appends.push(content);
        }
    }

    replacements.sort_by_key(|(start, _, _)| *start);

    let mut updated = current.to_owned();
    for (start, end, content) in replacements.into_iter().rev() {
        updated.replace_range(start..end, &content);
    }

    for content in appends {
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&content);
    }

    Ok(updated)
}

fn get_subscription_section<'a>(subscription: &'a Mapping, section: &str) -> Result<&'a Value> {
    let key = Value::String(section.to_owned());
    let value = subscription
        .get(&key)
        .ok_or_else(|| anyhow!("Subscription config does not contain `{section}`"))?;
    if !value.is_sequence() {
        bail!("Subscription config `{section}` must be a list");
    }
    Ok(value)
}

fn render_section(section: &str, value: &Value) -> Result<String> {
    let mut mapping = Mapping::new();
    mapping.insert(Value::String(section.to_owned()), value.clone());

    let mut section = serde_yml::to_string(&Value::Mapping(mapping))
        .context("Fail to serialize subscription section")?;
    if !section.ends_with('\n') {
        section.push('\n');
    }

    Ok(section)
}

fn find_top_level_section_range(input: &str, section: &str) -> Option<(usize, usize)> {
    let line_ranges = line_ranges(input);
    let start_idx = line_ranges
        .iter()
        .position(|(start, end)| is_section_header(&input[*start..*end], section))?;
    let start = line_ranges[start_idx].0;
    let end = line_ranges
        .iter()
        .skip(start_idx + 1)
        .find(|(start, end)| is_top_level_key(&input[*start..*end]))
        .map_or(input.len(), |(start, _)| *start);

    Some((start, end))
}

fn line_ranges(input: &str) -> Vec<(usize, usize)> {
    if input.is_empty() {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut start = 0;
    for (idx, ch) in input.char_indices() {
        if ch == '\n' {
            ranges.push((start, idx + 1));
            start = idx + 1;
        }
    }
    if start < input.len() {
        ranges.push((start, input.len()));
    }

    ranges
}

fn is_section_header(line: &str, section: &str) -> bool {
    let line = line.trim_end_matches(['\r', '\n']);
    line.strip_prefix(section)
        .and_then(|rest| rest.strip_prefix(':'))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t', '#']))
}

fn is_top_level_key(line: &str) -> bool {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() || line.starts_with([' ', '\t', '#', '-']) {
        return false;
    }

    let Some((key, _)) = line.split_once(':') else {
        return false;
    };

    !key.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(input: &str) -> Value {
        serde_yml::from_str(input).unwrap()
    }

    fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
        let key = Value::String(key.to_owned());
        value.as_mapping().unwrap().get(&key).unwrap()
    }

    #[test]
    fn make_meta_url_adds_flag() {
        let url =
            Url::parse("https://example.com/api/v1/client/subscribe?token=abc&foo=bar").unwrap();

        assert_eq!(
            make_meta_url(url).as_str(),
            "https://example.com/api/v1/client/subscribe?token=abc&foo=bar&flag=meta"
        );
    }

    #[test]
    fn make_meta_url_replaces_existing_flag() {
        let url =
            Url::parse("https://example.com/api/v1/client/subscribe?token=abc&flag=clash").unwrap();

        assert_eq!(
            make_meta_url(url).as_str(),
            "https://example.com/api/v1/client/subscribe?token=abc&flag=meta"
        );
    }

    #[test]
    fn replace_subscription_sections_only_updates_subscription_keys() {
        let current = r#"
mixed-port: 7890
allow-lan: true
dns:
  enable: true
# keep before proxies
proxies:
  - name: old
    type: direct
proxy-groups:
  - name: old-group
    type: select
    proxies:
      - old
rules:
  - MATCH,old
profile:
  store-selected: true
"#;
        let subscription = yaml(
            r#"
mixed-port: 9999
allow-lan: false
dns:
  enable: false
proxies:
  - name: new
    type: direct
proxy-groups:
  - name: new-group
    type: select
    proxies:
      - new
rules:
  - MATCH,new
"#,
        );

        let updated = replace_subscription_sections(current, &subscription).unwrap();
        let updated_value = yaml(&updated);

        assert!(updated.contains("mixed-port: 7890\n"));
        assert!(updated.contains("allow-lan: true\n"));
        assert!(updated.contains("dns:\n  enable: true\n"));
        assert!(updated.contains("# keep before proxies\n"));
        assert!(updated.contains("profile:\n  store-selected: true\n"));
        assert!(!updated.contains("mixed-port: 9999\n"));
        assert_eq!(
            get(&updated_value, "mixed-port"),
            &Value::Number(7890.into())
        );
        assert_eq!(get(&updated_value, "allow-lan"), &Value::Bool(true));
        assert_eq!(
            get(&updated_value, "dns"),
            get(&yaml("dns:\n  enable: true\n"), "dns")
        );
        assert_eq!(
            get(&updated_value, "proxies"),
            get(&subscription, "proxies")
        );
        assert_eq!(
            get(&updated_value, "proxy-groups"),
            get(&subscription, "proxy-groups")
        );
        assert_eq!(get(&updated_value, "rules"), get(&subscription, "rules"));
    }

    #[test]
    fn replace_subscription_sections_requires_all_sections() {
        let current = "{}";
        let subscription = yaml(
            r#"
proxies: []
proxy-groups: []
"#,
        );

        assert!(replace_subscription_sections(current, &subscription).is_err());
    }
}
