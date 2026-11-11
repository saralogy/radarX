package app.survivaldisc

import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import java.io.ByteArrayInputStream
import uniffi.survival_disc.Disc
import uniffi.survival_disc.DiscException
import uniffi.survival_disc.ReaderOptions

/**
 * Serves every WebView request from the Rust core.
 *
 * Articles live at https://<dataset-id>.disc.invalid/<path>. A real https
 * origin (rather than a custom scheme) gives Chromium standard relative-URL
 * resolution; `.invalid` can never resolve and the app holds no INTERNET
 * permission, so nothing leaves the device either way.
 */
class DiscWebViewClient(
    private val disc: Disc,
    private val options: () -> ReaderOptions,
    private val onExternalLink: (String) -> Unit = {},
) : WebViewClient() {

    // Called on a WebView IO thread: a synchronous call into Rust is correct here.
    override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse {
        val url = request.url
        val dataset = url.host?.removeSuffix(HOST_SUFFIX)?.takeIf { url.host!!.endsWith(HOST_SUFFIX) }
            ?: return status(403, "Forbidden")
        val pathAndQuery = (url.encodedPath ?: "/").removePrefix("/") +
            (url.encodedQuery?.let { "?$it" } ?: "")
        return try {
            val r = disc.getResource(dataset, pathAndQuery, options())
            val mime = r.mime.substringBefore(';').trim()
            val charset = if (mime.startsWith("text/")) "utf-8" else null
            WebResourceResponse(mime, charset, ByteArrayInputStream(r.data))
        } catch (e: DiscException.NotFound) {
            page(404, "Not in this disc", "This page is not part of the installed library.")
        } catch (e: DiscException.Blocked) {
            status(403, "Forbidden")
        } catch (e: DiscException) {
            page(500, "Cannot open page", e.message ?: "The archive could not be read.")
        }
    }

    // Anything that is not disc content never navigates.
    override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
        val host = request.url.host ?: return true
        if (host.endsWith(HOST_SUFFIX)) return false
        onExternalLink(request.url.toString())
        return true
    }

    private fun page(code: Int, title: String, message: String): WebResourceResponse {
        val html = disc.messagePage(title, message, options()).toByteArray()
        return WebResourceResponse("text/html", "utf-8", code, title, emptyMap(), ByteArrayInputStream(html))
    }

    private fun status(code: Int, reason: String) =
        WebResourceResponse("text/plain", "utf-8", code, reason, emptyMap(), ByteArrayInputStream(ByteArray(0)))

    companion object {
        const val HOST_SUFFIX = ".disc.invalid"
        fun url(datasetId: String, urlPath: String) = "https://$datasetId$HOST_SUFFIX/$urlPath"
    }
}
