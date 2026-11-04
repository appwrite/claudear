#[derive(Debug)]
pub(crate) struct Statements {
    pub(crate) table: &'static str,
    pub(crate) minimum_capacity: usize,
    pub(crate) drop: &'static str,
    pub(crate) insert: &'static str,
    pub(crate) delete: &'static str,
    pub(crate) batch: &'static str,
    pub(crate) count: &'static str,
    pub(crate) newest: &'static str,
}

macro_rules! statements {
    ($table:literal, $source:literal, $embedding:literal, $minimum_capacity:expr) => {
        Statements {
            table: $table,
            minimum_capacity: $minimum_capacity,
            drop: concat!("DROP TABLE IF EXISTS ", $table),
            insert: concat!("INSERT INTO ", $table, "(rowid, embedding) VALUES (?1, ?2)"),
            delete: concat!("DELETE FROM ", $table, " WHERE rowid = ?1"),
            batch: concat!(
                "SELECT id, ",
                $embedding,
                " FROM ",
                $source,
                " WHERE id > ?1 AND ",
                $embedding,
                " IS NOT NULL AND length(",
                $embedding,
                ") = ?2 ORDER BY id LIMIT ?3"
            ),
            count: concat!(
                "SELECT count(*) FROM ",
                $source,
                " WHERE ",
                $embedding,
                " IS NOT NULL AND length(",
                $embedding,
                ") = ?1"
            ),
            newest: concat!(
                "SELECT length(",
                $embedding,
                ") FROM ",
                $source,
                " WHERE ",
                $embedding,
                " IS NOT NULL ORDER BY id DESC LIMIT 1"
            ),
        }
    };
}

pub(crate) use statements;
