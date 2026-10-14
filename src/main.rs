mod api;
mod app;
mod db;
mod feeds;
mod launch;
mod manifest;
mod patcher;
mod paths;
mod session;
mod settings;
mod ui;

use std::io::stdout;

use anyhow::Result;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;

fn main() -> Result<()> {
    let db = db::Db::open(&paths::database_file()?)?;
    let mut app = app::App::new(db, api::Api::new()?)?;

    let mut terminal = ratatui::init();
    let result = execute!(stdout(), EnableMouseCapture)
        .map_err(anyhow::Error::from)
        .and_then(|()| app.run(&mut terminal));
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}
