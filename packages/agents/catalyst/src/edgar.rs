//! The SEC's EDGAR filings: free, public, and published as they are
//! accepted. Each company's recent filings are one JSON document at
//! `data.sec.gov/submissions/CIK##########.json`.

use serde_json::Value;

/// One current report (Form 8-K) as the SEC lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub accession: String,
    /// When EDGAR accepted it, as the SEC writes it (ISO 8601, US Eastern).
    pub accepted: String,
    /// The report's item numbers, such as `2.02` (results of operations).
    pub items: Vec<String>,
}

/// The path of a company's submissions document.
pub fn path(cik: u64) -> String {
    format!("/submissions/CIK{cik:010}.json")
}

/// The 8-Ks among a company's recent filings, newest first.
pub fn current_reports(document: &Value) -> Vec<Report> {
    let recent = &document["filings"]["recent"];
    let column = |name: &str| recent[name].as_array().cloned().unwrap_or_default();
    let (forms, accessions, accepted, items) = (
        column("form"),
        column("accessionNumber"),
        column("acceptanceDateTime"),
        column("items"),
    );
    forms
        .iter()
        .enumerate()
        .filter(|(_, form)| form.as_str() == Some("8-K"))
        .filter_map(|(i, _)| {
            Some(Report {
                accession: accessions.get(i)?.as_str()?.to_string(),
                accepted: accepted.get(i)?.as_str().unwrap_or_default().to_string(),
                items: items
                    .get(i)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .split(',')
                    .map(|item| item.trim().to_string())
                    .filter(|item| !item.is_empty())
                    .collect(),
            })
        })
        .collect()
}

/// GET `https://data.sec.gov{path}` through the host's `wasi:http`, with
/// the User-Agent the SEC asks for.
#[cfg(target_arch = "wasm32")]
pub fn get(path: &str, user_agent: &str) -> Result<Value, String> {
    use strategy_sdk::http::outgoing_handler;
    use strategy_sdk::http::types::{Fields, Method, OutgoingRequest, Scheme};

    let headers = Fields::new();
    headers
        .append("user-agent", user_agent.as_bytes())
        .map_err(|e| format!("{e:?}"))?;
    headers
        .append("accept", b"application/json")
        .map_err(|e| format!("{e:?}"))?;
    let request = OutgoingRequest::new(headers);
    request.set_method(&Method::Get).map_err(|_| "method")?;
    request
        .set_scheme(Some(&Scheme::Https))
        .map_err(|_| "scheme")?;
    request
        .set_authority(Some("data.sec.gov"))
        .map_err(|_| "authority")?;
    request
        .set_path_with_query(Some(path))
        .map_err(|_| "path")?;
    let pending = outgoing_handler::handle(request, None).map_err(|e| format!("{e:?}"))?;
    pending.subscribe().block();
    let response = pending
        .get()
        .ok_or("no response")?
        .map_err(|_| "response taken twice")?
        .map_err(|e| format!("{e:?}"))?;
    if response.status() != 200 {
        return Err(format!("the SEC answered {}", response.status()));
    }
    let body = response.consume().map_err(|_| "no body")?;
    let stream = body.stream().map_err(|_| "no body stream")?;
    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(strategy_sdk::bindings::wasi::io::streams::StreamError::Closed) => break,
            Err(e) => return Err(format!("{e:?}")),
        }
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("the document is too large".into());
        }
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

/// Off WebAssembly (the agent's tests), there is no host to ask.
#[cfg(not(target_arch = "wasm32"))]
pub fn get(_path: &str, _user_agent: &str) -> Result<Value, String> {
    Err("no host".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_current_reports_are_read_with_their_items() {
        let document = serde_json::json!({"filings": {"recent": {
            "form": ["10-Q", "8-K", "4", "8-K"],
            "accessionNumber": ["a", "b", "c", "d"],
            "acceptanceDateTime": ["t1", "t2", "t3", "t4"],
            "items": ["", "2.02,9.01", "", "5.02"]
        }}});
        let reports = current_reports(&document);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].accession, "b");
        assert_eq!(reports[0].items, vec!["2.02", "9.01"]);
        assert_eq!(reports[1].items, vec!["5.02"]);
    }

    #[test]
    fn a_companys_document_is_named_by_its_ten_digit_key() {
        assert_eq!(path(1_045_810), "/submissions/CIK0001045810.json");
    }
}
