import { Database } from "bun:sqlite";
import { mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { drizzle } from "drizzle-orm/bun-sqlite";
import { migrate } from "drizzle-orm/bun-sqlite/migrator";
import * as schema from "./schema";

// Use a data directory for the DB file
const dbPath = process.env.DB_PATH || join(process.cwd(), "data/sunrise.db");

// Auto-create parent directory if it doesn't exist
mkdirSync(dirname(dbPath), { recursive: true });

const sqlite = new Database(dbPath);
sqlite.run("PRAGMA foreign_keys = ON;");

export const db = drizzle(sqlite, { schema });

// Apply committed migrations at startup. The folder is resolved relative to
// this file so it works regardless of the process working directory.
migrate(db, { migrationsFolder: join(import.meta.dir, "../../drizzle") });
