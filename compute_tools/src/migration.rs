use postgres::Client;
use tracing::{error, info};

pub(crate) struct MigrationRunner<'m> {
    client: &'m mut Client,
    migrations: &'m [&'m str],
}

impl<'m> MigrationRunner<'m> {
    pub fn new(client: &'m mut Client, migrations: &'m [&'m str]) -> Self {
        // The neon_migration.migration_id::id column is a bigint, which is equivalent to an i64
        assert!(migrations.len() + 1 < i64::MAX as usize);

        Self { client, migrations }
    }

    fn get_migration_id(&mut self) -> Result<i64, postgres::Error> {
        let query = "SELECT id FROM neon_migration.migration_id";
        let row = self.client.query_one(query, &[])?;

        Ok(row.get::<&str, i64>("id"))
    }

    fn update_migration_id(&mut self, migration_id: i64) -> Result<(), postgres::Error> {
        let setval = format!("UPDATE neon_migration.migration_id SET id={}", migration_id);

        self.client.simple_query(&setval)?;

        Ok(())
    }

    fn prepare_migrations(&mut self) -> Result<(), postgres::Error> {
        let query = "CREATE SCHEMA IF NOT EXISTS neon_migration";
        self.client.simple_query(query)?;

        let query = "CREATE TABLE IF NOT EXISTS neon_migration.migration_id (key INT NOT NULL PRIMARY KEY, id bigint NOT NULL DEFAULT 0)";
        self.client.simple_query(query)?;

        let query = "INSERT INTO neon_migration.migration_id VALUES (0, 0) ON CONFLICT DO NOTHING";
        self.client.simple_query(query)?;

        let query = "ALTER SCHEMA neon_migration OWNER TO cloud_admin";
        self.client.simple_query(query)?;

        let query = "REVOKE ALL ON SCHEMA neon_migration FROM PUBLIC";
        self.client.simple_query(query)?;

        Ok(())
    }

    pub fn run_migrations(mut self) -> Result<(), postgres::Error> {
        if let Err(e) = self.prepare_migrations() {
            error!("Failed to prepare the migration relations: {}", e);
            return Err(e);
        }

        let mut current_migration = match self.get_migration_id() {
            Ok(id) => id as usize,
            Err(e) => {
                error!("Failed to get the current migration id: {}", e);
                return Err(e);
            }
        };

        while current_migration < self.migrations.len() {
            macro_rules! migration_id {
                ($cm:expr) => {
                    ($cm + 1) as i64
                };
            }

            let migration = self.migrations[current_migration];

            if migration.starts_with("-- SKIP") {
                info!("Skipping migration id={}", migration_id!(current_migration));
            } else {
                info!(
                    "Running migration id={}:\n{}\n",
                    migration_id!(current_migration),
                    migration
                );

                if let Err(e) = self.client.simple_query("BEGIN") {
                    error!("Failed to begin the migration transaction: {}", e);
                    return Err(e);
                }

                if let Err(e) = self.client.simple_query(migration) {
                    error!("Failed to run the migration: {}", e);
                    return Err(e);
                }

                // Migration IDs start at 1
                if let Err(e) = self.update_migration_id(migration_id!(current_migration)) {
                    error!(
                        "Failed to update the migration id to {}: {}",
                        migration_id!(current_migration),
                        e
                    );
                    return Err(e);
                }

                if let Err(e) = self.client.simple_query("COMMIT") {
                    error!("Failed to commit the migration transaction: {}", e);
                    return Err(e);
                }

                info!("Finished migration id={}", migration_id!(current_migration));
            }

            current_migration += 1;
        }

        Ok(())
    }
}
