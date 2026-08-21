/**
 * Test-only database helpers (Node/vitest).
 *
 * Builds an in-memory better-sqlite3 database whose schema comes from the
 * committed drizzle migration SQL files, so tests share a single source of
 * DDL truth with the runtime database.
 */
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import Database from "better-sqlite3";
import { drizzle } from "drizzle-orm/better-sqlite3";
import * as schema from "./schema";

const migrationsDir = fileURLToPath(new URL("../../drizzle", import.meta.url));

/**
 * Executes every committed migration SQL file (in order) against the given
 * better-sqlite3 database.
 */
export function applyMigrations(sqlite: { exec: (sql: string) => unknown }) {
    const files = readdirSync(migrationsDir)
        .filter((file) => file.endsWith(".sql"))
        .sort();

    for (const file of files) {
        const sql = readFileSync(join(migrationsDir, file), "utf8");
        for (const statement of sql.split("--> statement-breakpoint")) {
            const trimmed = statement.trim();
            if (trimmed) {
                sqlite.exec(trimmed);
            }
        }
    }
}

/**
 * Creates an in-memory database with the full migrated schema, wrapped in a
 * drizzle instance compatible with the mocked "../db" module shape.
 */
export function createTestDb() {
    const sqlite = new Database(":memory:");
    sqlite.pragma("foreign_keys = ON");
    applyMigrations(sqlite);
    const db = drizzle(sqlite, { schema });
    return { sqlite, db };
}
