import { mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import Database from "better-sqlite3";
import { drizzle } from "drizzle-orm/better-sqlite3";
import * as schema from "./schema";

// Use a data directory for the DB file
const dbPath = process.env.DB_PATH || join(process.cwd(), "data/sqlite.db");

// Auto-create parent directory if it doesn't exist
mkdirSync(dirname(dbPath), { recursive: true });

const sqlite = new Database(dbPath);
export const db = drizzle(sqlite, { schema });
