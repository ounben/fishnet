use std::{
    cmp::{max, min},
    fmt,
    fs::{File, OpenOptions},
    io,
    io::{Read as _, Seek as _, Write as _},
    num::NonZeroUsize,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use rusqlite::{params, Connection, Result}; // SQLite-Bibliothek

use crate::configure::StatsOpt;

fn default_stats_file() -> Option<PathBuf> {
    home::home_dir().map(|dir| dir.join(".fishnet-stats"))
}

pub struct StatsRecorder {
    pub stats: Stats,
    pub nnue_nps: NpsRecorder,
    store: Option<(PathBuf, File)>,
    cores: NonZeroUsize,
    db_conn: Option<Connection>, // SQLite-Verbindung
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub total_batches: u64,
    pub total_positions: u64,
    pub total_nodes: u64,
}

impl Stats {
    fn load_from(file: &mut File) -> io::Result<Option<Stats>> {
        file.rewind()?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(if buf.is_empty() {
            None
        } else {
            Some(
                serde_json::from_slice(&buf)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?,
            )
        })
    }

    fn save_to(&self, file: &mut File) -> io::Result<()> {
        file.set_len(0)?;
        file.rewind()?;
        file.write_all(
            serde_json::to_string_pretty(&self)
                .expect("serialize stats")
                .as_bytes(),
        )?;
        Ok(())
    }
}

impl StatsRecorder {
    pub fn new(opt: StatsOpt, cores: NonZeroUsize) -> StatsRecorder {
        let nnue_nps = NpsRecorder::new();

        if opt.no_stats_file {
            return StatsRecorder {
                stats: Stats::default(),
                store: None,
                nnue_nps,
                cores,
                db_conn: None,
            };
        }

        let path = opt.stats_file.or_else(default_stats_file);

        let (stats, store) = match &path {
            Some(path) => match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
            {
                Ok(mut file) => (
                    match Stats::load_from(&mut file) {
                        Ok(Some(stats)) => {
                            println!("Resuming from {path:?} ...");
                            stats
                        }
                        Ok(None) => {
                            println!("Recording to new stats file {path:?} ...");
                            Stats::default()
                        }
                        Err(err) => {
                            eprintln!("E: Failed to resume from {path:?}: {err}. Resetting ...");
                            Stats::default()
                        }
                    },
                    Some((path.clone(), file)),
                ),
                Err(err) => {
                    eprintln!("E: Failed to open {path:?}: {err}");
                    (Stats::default(), None)
                }
            },
            None => {
                eprintln!("E: Could not resolve ~/.fishnet-stats");
                (Stats::default(), None)
            }
        };

        // SQLite-Datenbank initialisieren
        let db_conn = match initialize_database("stats.db") {
            Ok(conn) => Some(conn),
            Err(err) => {
                eprintln!("E: Failed to initialize SQLite database: {err}");
                None
            }
        };

        StatsRecorder {
            stats,
            store,
            nnue_nps,
            cores,
            db_conn,
        }
    }

    pub fn record_batch(&mut self, positions: u64, nodes: u64, nnue_nps: Option<u32>) {
        self.stats.total_batches += 1;
        self.stats.total_positions += positions;
        self.stats.total_nodes += nodes;

        if let Some(nnue_nps) = nnue_nps {
            self.nnue_nps.record(nnue_nps);
        }

        // Speichern in .stats-file
        if let Some((ref path, ref mut stats_file)) = &self.store {
            if let Err(err) = self.stats.save_to(stats_file) {
                eprintln!("E: Failed to write stats to {path:?}: {err}");
            }
        }

        // Speichern in SQLite-Datenbank
        if let Some(conn) = &self.db_conn {
            if let Err(err) = self.save_to_database(conn, nnue_nps) {
                eprintln!("E: Failed to save stats to SQLite database: {err}");
            }
        }
    }

    // Neue Methode: Stats in SQLite speichern
    pub fn save_to_database(&self, conn: &Connection, nnue_nps: Option<u32>) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_secs();

        conn.execute(
            "INSERT INTO stats (timestamp, total_batches, total_positions, total_nodes, nnue_nps)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                now as i64,
                self.stats.total_batches as i64,
                self.stats.total_positions as i64,
                self.stats.total_nodes as i64,
                nnue_nps.unwrap_or_default() as i64, // nnue_nps, falls vorhanden
            ],
        )?;
        Ok(())
    }
}

// Funktion, um die SQLite-Datenbank zu initialisieren
fn initialize_database(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS stats (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp INTEGER NOT NULL,
            total_batches INTEGER NOT NULL,
            total_positions INTEGER NOT NULL,
            total_nodes INTEGER NOT NULL,
            nnue_nps INTEGER NOT NULL
        )",
        [],
    )?;
    Ok(conn)
}

#[derive(Clone)]
pub struct NpsRecorder {
    pub nps: u32,
    pub uncertainty: f64,
}

impl NpsRecorder {
    fn new() -> NpsRecorder {
        NpsRecorder {
            nps: 400_000, // start with an optimistic estimate
            uncertainty: 1.0,
        }
    }

    fn record(&mut self, nps: u32) {
        let alpha = 0.9;
        self.uncertainty *= alpha;
        self.nps = (f64::from(self.nps) * alpha + f64::from(nps) * (1.0 - alpha)) as u32;
    }
}

impl fmt::Display for NpsRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} knps/core", self.nps / 1000)?;
        if self.uncertainty > 0.1 {
            write!(f, " ?")?;
        }
        if self.uncertainty > 0.4 {
            write!(f, "?")?;
        }
        if self.uncertainty > 0.7 {
            write!(f, "?")?;
        }
        Ok(())
    }
}
