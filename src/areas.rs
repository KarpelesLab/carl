//! Tool areas: Carl's tools are grouped by area, and each session exposes only
//! the areas it enabled (`carl_enable`), so agents aren't handed dozens of
//! tools they won't use. `carl_*` tools are always visible.
//!
//! Enabling an area changes the session's tool list and sends MCP's
//! `tools/list_changed`, which clients answer by fetching the list again.
//! Clients that ignore that notification can preset areas with `CARL_AREAS`.

use std::collections::BTreeSet;

/// An area of tools.
pub struct Area {
    pub id: &'static str,
    pub description: &'static str,
    /// Whether it works yet; scaffolded areas can't be enabled.
    pub available: bool,
}

pub const AREAS: &[Area] = &[
    Area {
        id: "agents",
        description: "See the other AI agents on this machine, say what you're working on, message them.",
        available: true,
    },
    Area {
        id: "google",
        description: "Link and manage Google accounts. Enabled with any google.* area. Linking again with more areas (e.g. meet) adds them.",
        available: true,
    },
    Area {
        id: "google.mail",
        description: "Gmail: search, read, labels, drafts.",
        available: true,
    },
    Area {
        id: "google.calendar",
        description: "Google Calendar: events, free/busy, create events.",
        available: true,
    },
    Area {
        id: "google.drive",
        description: "Google Drive: search and read Docs/Sheets/Slides/files, create private files.",
        available: true,
    },
    Area {
        id: "google.contacts",
        description: "Google Contacts: search.",
        available: true,
    },
    Area {
        id: "google.meet",
        description: "Google Meet: create meeting links, past meetings, participants, transcripts, recordings.",
        available: true,
    },
    Area {
        id: "klb",
        description: "KarpelesLab / AtOnline platform (hub.atonline.com): log in, call its REST API, upload files.",
        available: true,
    },
    Area {
        id: "wallet",
        description: "Crypto wallet: balances, addresses, sending. Not available yet.",
        available: false,
    },
    Area {
        id: "email",
        description: "Carl-managed email addresses. Not available yet.",
        available: false,
    },
];

/// Areas enabled in a new session unless `CARL_AREAS` says otherwise.
const DEFAULT: &[&str] = &["agents"];

/// The area a tool belongs to; `None` for the always-visible `carl_*` tools.
pub fn area_of(tool: &str) -> Option<&'static str> {
    const PREFIXES: &[(&str, &str)] = &[
        ("carl_", ""),
        ("agent_", "agents"),
        ("google_mail_", "google.mail"),
        ("google_calendar_", "google.calendar"),
        ("google_drive_", "google.drive"),
        ("google_contacts_", "google.contacts"),
        ("google_meet_", "google.meet"),
        ("google_", "google"),
        ("klb_", "klb"),
        ("wallet_", "wallet"),
        ("email_", "email"),
    ];
    PREFIXES
        .iter()
        .find(|(prefix, _)| tool.starts_with(prefix))
        .and_then(|(_, area)| (!area.is_empty()).then_some(*area))
}

#[cfg(test)]
pub fn find(id: &str) -> Option<&'static Area> {
    AREAS.iter().find(|a| a.id == id)
}

/// Expand requested ids: `google` means every Google area, and any `google.*`
/// area brings the account tools (`google`) along.
pub fn expand(ids: &[&str]) -> Result<BTreeSet<&'static str>, String> {
    let mut out = BTreeSet::new();
    for id in ids {
        let id = id.trim();
        let matching: Vec<&'static Area> = if id == "all" {
            AREAS.iter().filter(|a| a.available).collect()
        } else {
            AREAS
                .iter()
                .filter(|a| a.id == id || a.id.starts_with(&format!("{id}.")))
                .collect()
        };
        if matching.is_empty() {
            return Err(format!("unknown area `{id}`; carl_status lists them"));
        }
        for area in matching {
            if !area.available {
                return Err(format!("the {} area isn't available yet", area.id));
            }
            out.insert(area.id);
            if area.id.starts_with("google.") {
                out.insert("google");
            }
        }
    }
    Ok(out)
}

/// The areas a new session starts with: `$CARL_AREAS` (comma-separated ids,
/// or `all`), else [`DEFAULT`].
pub fn initial(env: Option<&str>) -> BTreeSet<&'static str> {
    let Some(spec) = env.filter(|s| !s.trim().is_empty()) else {
        return expand(DEFAULT).expect("default areas are valid");
    };
    let ids: Vec<&str> = spec.split(',').filter(|s| !s.trim().is_empty()).collect();
    match expand(&ids) {
        Ok(areas) => areas,
        Err(e) => {
            tracing::warn!("ignoring CARL_AREAS: {e}");
            expand(DEFAULT).expect("default areas are valid")
        }
    }
}

/// Areas saved from an earlier session, exactly as they were (not expanded:
/// a saved `google` is the account tools that came with `google.mail`, not
/// every Google area), if they are all still valid.
pub fn restore(saved: &[String]) -> Option<BTreeSet<&'static str>> {
    let mut out = BTreeSet::new();
    for id in saved {
        let area = AREAS.iter().find(|a| a.id == id && a.available)?;
        out.insert(area.id);
        if area.id.starts_with("google.") {
            out.insert("google");
        }
    }
    Some(out)
}

/// Whether `tool` is visible with `enabled` areas.
pub fn visible(tool: &str, enabled: &BTreeSet<&'static str>) -> bool {
    area_of(tool).is_none_or(|area| enabled.contains(area))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_map_to_known_areas() {
        assert_eq!(area_of("carl_status"), None);
        assert_eq!(area_of("agent_list"), Some("agents"));
        assert_eq!(area_of("google_link"), Some("google"));
        assert_eq!(area_of("google_mail_search"), Some("google.mail"));
        assert_eq!(area_of("wallet_send"), Some("wallet"));
        for area in AREAS {
            assert_eq!(find(area.id).unwrap().id, area.id);
        }
    }

    #[test]
    fn expanding_areas() {
        let google = expand(&["google"]).unwrap();
        assert_eq!(google.len(), 6, "{google:?}");
        let mail = expand(&["google.mail"]).unwrap();
        assert_eq!(mail, BTreeSet::from(["google", "google.mail"]));
        assert!(expand(&["wallet"]).unwrap_err().contains("isn't available"));
        assert!(expand(&["nope"]).unwrap_err().contains("unknown"));
        assert!(!expand(&["all"]).unwrap().contains("wallet"));
        // A prefix must end at a dot: "goo" isn't "google".
        assert!(expand(&["goo"]).is_err());
    }

    #[test]
    fn initial_areas() {
        assert_eq!(initial(None), BTreeSet::from(["agents"]));
        assert_eq!(initial(Some("")), BTreeSet::from(["agents"]));
        assert_eq!(
            initial(Some("agents, google.drive")),
            BTreeSet::from(["agents", "google", "google.drive"])
        );
        assert_eq!(initial(Some("bogus")), BTreeSet::from(["agents"]));
        assert!(initial(Some("all")).contains("google.contacts"));
    }

    #[test]
    fn visibility() {
        let enabled = BTreeSet::from(["agents"]);
        assert!(visible("carl_status", &enabled));
        assert!(visible("agent_send", &enabled));
        assert!(!visible("google_mail_search", &enabled));
    }

    #[test]
    fn restoring_keeps_exactly_the_saved_areas() {
        let saved: Vec<String> = ["agents", "google", "google.mail"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            restore(&saved).unwrap(),
            BTreeSet::from(["agents", "google", "google.mail"])
        );
        assert!(restore(&["wallet".to_string()]).is_none(), "unavailable");
        assert!(restore(&["gone".to_string()]).is_none(), "unknown");
        assert_eq!(
            restore(&["google.drive".to_string()]).unwrap(),
            BTreeSet::from(["google", "google.drive"])
        );
    }
}
