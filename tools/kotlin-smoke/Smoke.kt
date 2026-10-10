// The Android seam's smoke test, run on a JVM rather than an emulator.
//
// `mise run kotlin-smoke` compiles this against the Kotlin UniFFI generates
// from `sunrise-core-bindings` and runs it with the host's build of that crate
// on JNA's library path. It proves the generated Kotlin compiles, that JNA
// loads the library and agrees with it on the checksums of every exported
// function, and that one async call each way crosses the seam: a constructor
// that creates a SQLCipher vault, and a query against it. It proves nothing
// about Android itself: no Android target is compiled, and no ART, Bionic or
// NDK linker is involved. `docs/07-clients/mobile-android.md` says the same.

import kotlin.system.exitProcess
import kotlinx.coroutines.runBlocking
import uniffi.sunrise_core_bindings.CoreQuery
import uniffi.sunrise_core_bindings.CoreQueryResult
import uniffi.sunrise_core_bindings.SunriseCore

fun main() {
    val vault = kotlin.io.path.createTempDirectory("sunrise-kotlin-smoke").toFile()
    try {
        runBlocking {
            // A fixed, non-secret root: the vault lives for this process only.
            val root = ByteArray(32) { 7 }
            val core = SunriseCore.open(vault.path, root, "0.0.0+kotlin-smoke")
            try {
                val inbox = core.query(CoreQuery.Inbox)
                check(inbox is CoreQueryResult.Tasks) { "the Inbox answered $inbox" }
                check(inbox.tasks.isEmpty()) { "a fresh vault's Inbox holds ${inbox.tasks}" }
            } finally {
                core.shutdown()
                core.close()
            }
        }
    } catch (e: Throwable) {
        System.err.println("kotlin-smoke: FAILED: $e")
        e.printStackTrace()
        exitProcess(1)
    } finally {
        vault.deleteRecursively()
    }
    println("kotlin-smoke: opened a vault and queried it through the generated Kotlin")
}
