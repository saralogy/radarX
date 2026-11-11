package app.survivaldisc

import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import uniffi.survival_disc.Dataset
import uniffi.survival_disc.Disc
import uniffi.survival_disc.SearchResult

/**
 * Owns the Rust [Disc] handle. All calls hop to Dispatchers.IO; the Rust
 * core is synchronous and thread-safe.
 */
class DiscRepository private constructor(val disc: Disc, private val context: Context) {

    suspend fun search(query: String, limit: UInt = 10u): List<SearchResult> =
        withContext(Dispatchers.IO) { disc.search(query, limit) }

    suspend fun datasets(): List<Dataset> = withContext(Dispatchers.IO) { disc.datasets() }

    /**
     * Side-load a ZIM picked with ACTION_OPEN_DOCUMENT. The archive is read in
     * place through its file descriptor (no copy, so a 90 GB ZIM on an SD card
     * costs no internal storage). Persist the URI permission and call this
     * again on each launch.
     */
    suspend fun sideLoad(uri: Uri): Dataset = withContext(Dispatchers.IO) {
        context.contentResolver.takePersistableUriPermission(uri, android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION)
        val name = context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
            ?.use { c -> if (c.moveToFirst()) c.getString(0) else null }
            ?.removeSuffix(".zim") ?: "zim"
        val pfd = context.contentResolver.openFileDescriptor(uri, "r")
            ?: error("cannot open $uri")
        disc.addArchiveFd(pfd.detachFd(), name) // Rust now owns and closes the fd
    }

    companion object {
        /**
         * @param discDir directory holding disc.json, the ZIMs and search-index/.
         *   For Play Asset Delivery use a fast-follow pack and pass
         *   AssetPackManager.getPackLocation(pack).assetsPath(); install-time
         *   packs are not exposed as files and would need a full copy.
         */
        suspend fun open(context: Context, discDir: String?): DiscRepository = withContext(Dispatchers.IO) {
            val disc = if (discDir != null) Disc.open(discDir) else Disc.empty()
            // ~1/8 of the app heap class keeps 2 GB devices comfortable.
            disc.setCacheBudget((Runtime.getRuntime().maxMemory() / 8).toULong())
            DiscRepository(disc, context.applicationContext)
        }
    }
}
