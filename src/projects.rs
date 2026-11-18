//! Projects group tickets across repos; they carry a Palaver link and default service panes.
use crate::tickets::{self, Ticket};
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Service {
    pub name: String,
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

pub fn exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT 1 FROM projects WHERE name=?", [name], |_| Ok(())).optional()?.is_some())
}

pub fn list(conn: &Connection) -> Result<Vec<Value>> {
    crate::db::rows(
        conn,
        "SELECT p.name, p.palaver,
           (SELECT count(*) FROM tickets i WHERE i.project=p.name) tickets,
           (SELECT count(*) FROM tickets i WHERE i.project=p.name AND i.status IN ('todo','backlog')) planned,
           (SELECT count(*) FROM tickets i WHERE i.project=p.name AND i.status='doing') doing,
           (SELECT count(*) FROM tickets i WHERE i.project=p.name AND i.status='done') done
         FROM projects p ORDER BY name",
        [],
    )
}

pub fn link_palaver(conn: &Connection, project: &str, palaver: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO projects(name, palaver) VALUES(?,?) ON CONFLICT(name) DO UPDATE SET palaver=excluded.palaver",
        params![project, palaver],
    )?;
    Ok(())
}

pub fn palaver_of(conn: &Connection, project: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT palaver FROM projects WHERE name=?", [project], |r| r.get(0)).optional()?.flatten())
}

pub fn set_services(conn: &Connection, project: &str, services: &[Service]) -> Result<()> {
    conn.execute(
        "INSERT INTO projects(name, services) VALUES(?,?) ON CONFLICT(name) DO UPDATE SET services=excluded.services",
        params![project, serde_json::to_string(services)?],
    )?;
    Ok(())
}

pub fn services(conn: &Connection, project: &str) -> Result<Vec<Service>> {
    let raw: Option<String> =
        conn.query_row("SELECT services FROM projects WHERE name=?", [project], |r| r.get(0)).optional()?.flatten();
    Ok(raw.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or_default())
}

/// The repo holding most of the project's tickets.
pub fn main_repo(conn: &Connection, project: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT repo FROM tickets WHERE project=? GROUP BY repo ORDER BY count(*) DESC LIMIT 1",
            [project],
            |r| r.get(0),
        )
        .optional()?)
}

/// Planned tickets by cluster (A-Z, unclustered last), then rank.
pub fn sort_planned(v: &mut [Ticket]) {
    v.sort_by(|a, b| {
        a.cluster
            .is_none()
            .cmp(&b.cluster.is_none())
            .then_with(|| a.cluster.cmp(&b.cluster))
            .then_with(|| a.rank.total_cmp(&b.rank))
    });
}

/// Planned tickets grouped by cluster, in progress, recently done, live sessions, known clusters.
/// `live_sessions` comes from the summary so the session view stays in one place.
pub fn overview(conn: &Connection, project: &str, live_sessions: &[Value]) -> Result<Value> {
    let proj = crate::db::rows(conn, "SELECT * FROM projects WHERE name=?", [project])?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no project {project}"))?;
    let all: Vec<Ticket> = tickets::all(conn)?.into_iter().filter(|t| t.project.as_deref() == Some(project)).collect();
    let mut todo: Vec<Ticket> = all.iter().filter(|t| t.status == "todo" || t.status == "backlog").cloned().collect();
    sort_planned(&mut todo);
    let doing: Vec<&Ticket> = all.iter().filter(|t| t.status == "doing").collect();
    let mut done: Vec<&Ticket> = all.iter().filter(|t| t.status == "done").collect();
    done.sort_by(|a, b| b.updated.cmp(&a.updated));
    done.truncate(20);
    let mut clusters: Vec<&str> = all.iter().filter_map(|t| t.cluster.as_deref()).collect();
    clusters.sort();
    clusters.dedup();
    Ok(json!({
        "project": proj,
        "todo": todo,
        "doing": doing,
        "done": done,
        "sessions": live_sessions.iter().filter(|s| s["project"] == project).collect::<Vec<_>>(),
        "clusters": clusters,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::temp_db;
    use crate::tickets::{create, update, NewTicket};

    fn nt(title: &str, project: &str) -> NewTicket {
        NewTicket { repo: "r".into(), title: title.into(), project: Some(project.into()), ..Default::default() }
    }

    #[test]
    fn overview_clusters_with_unsorted_last() {
        let (_d, c) = temp_db();
        let a = create(&c, nt("a", "P")).unwrap();
        let b = create(&c, nt("b", "P")).unwrap();
        let d = create(&c, nt("c", "P")).unwrap();
        let e = create(&c, nt("d", "P")).unwrap();
        create(&c, nt("other", "Q")).unwrap();
        let set = |k: &str, f: Value| update(&c, k, f.as_object().unwrap(), None).unwrap();
        set(&a, json!({"cluster": "zeta"}));
        set(&b, json!({"cluster": "alpha"}));
        set(&d, json!({"status": "doing"}));
        set(&e, json!({"status": "done"}));
        let o = overview(&c, "P", &[json!({"project":"P","key":"x"}), json!({"project":"Q"})]).unwrap();
        let todo: Vec<_> =
            o["todo"].as_array().unwrap().iter().map(|t| t["key"].as_str().unwrap().to_string()).collect();
        assert_eq!(todo, vec![b, a]);
        assert_eq!(o["clusters"], json!(["alpha", "zeta"]));
        assert_eq!(o["doing"].as_array().unwrap().len(), 1);
        assert_eq!(o["done"].as_array().unwrap().len(), 1);
        assert_eq!(o["sessions"].as_array().unwrap().len(), 1);
        assert!(overview(&c, "nope", &[]).is_err());
    }

    #[test]
    fn unclustered_tickets_sort_after_clustered() {
        let mut v: Vec<Ticket> = ["x", "", "a"]
            .iter()
            .enumerate()
            .map(|(i, cl)| Ticket {
                key: format!("K-{i}"),
                repo: "r".into(),
                title: String::new(),
                body: String::new(),
                status: "todo".into(),
                priority: 3,
                project: None,
                cluster: (!cl.is_empty()).then(|| cl.to_string()),
                blocked_by: None,
                session: None,
                rank: i as f64,
                created: String::new(),
                updated: String::new(),
            })
            .collect();
        sort_planned(&mut v);
        assert_eq!(v.iter().map(|t| t.key.as_str()).collect::<Vec<_>>(), ["K-2", "K-0", "K-1"]);
    }

    #[test]
    fn services_and_palaver_link_roundtrip() {
        let (_d, c) = temp_db();
        let sv = vec![Service { name: "api".into(), cmd: "npm run api".into(), cwd: Some("server".into()) }];
        set_services(&c, "P", &sv).unwrap();
        link_palaver(&c, "P", "Palaver P").unwrap();
        assert_eq!(services(&c, "P").unwrap(), sv);
        assert_eq!(palaver_of(&c, "P").unwrap().as_deref(), Some("Palaver P"));
        assert!(services(&c, "none").unwrap().is_empty());
        let l = list(&c).unwrap();
        assert_eq!(l[0]["palaver"], "Palaver P");
    }
}
