//! `ferromesh contacts`: the companion radio's contact list, and pinning.

use anyhow::Result;
use ferromesh_model::{PinRequest, RadioContact};

use crate::channels::{json_lines, local, print, table};
use crate::render;
use crate::server::Server;

pub async fn list(server: &Server, json: bool) -> Result<()> {
    let contacts: Vec<RadioContact> = server.get("/api/v1/contacts").await?;
    if json {
        return json_lines(&contacts);
    }
    if contacts.is_empty() {
        render::status("the companion radio has no contacts");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = contacts
        .iter()
        .map(|contact| {
            vec![
                if contact.favourite { "★".into() } else { String::new() },
                contact.name.clone(),
                contact.kind.clone(),
                contact.pubkey[..8].to_owned(),
                contact.route_hops.map_or_else(|| "flood".into(), |hops| format!("{hops} hops")),
                contact.last_advert.map(local).unwrap_or_default(),
            ]
        })
        .collect();
    print(&table(&["", "NAME", "TYPE", "KEY", "ROUTE", "ADVERT CLOCK"], &rows, &[]))?;
    let favourites = contacts.iter().filter(|contact| contact.favourite).count();
    render::status(format_args!("{} contacts, {favourites} pinned (★)", contacts.len()));
    Ok(())
}

pub async fn pin(server: &Server, to: String, pinned: bool, token: Option<&str>) -> Result<()> {
    let contact: RadioContact =
        server.post("/api/v1/contacts", &PinRequest { to, pinned }, token).await?;
    let state = if contact.favourite { "pinned" } else { "unpinned" };
    print(&format!("{state} {} ({})\n", contact.name, &contact.pubkey[..8]))
}
