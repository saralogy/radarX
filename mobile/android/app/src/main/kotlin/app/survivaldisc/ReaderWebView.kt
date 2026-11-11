package app.survivaldisc

import android.annotation.SuppressLint
import android.webkit.WebSettings
import android.webkit.WebView
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.viewinterop.AndroidView
import uniffi.survival_disc.Disc
import uniffi.survival_disc.ReaderOptions

/** Locked-down article view. `url` comes from [DiscWebViewClient.url]. */
@SuppressLint("SetJavaScriptEnabled") // explicitly set to false below
@Composable
fun ReaderWebView(disc: Disc, url: String, options: ReaderOptions, modifier: Modifier = Modifier) {
    AndroidView(
        modifier = modifier,
        factory = { context ->
            WebView(context).apply {
                settings.apply {
                    javaScriptEnabled = false
                    allowFileAccess = false
                    allowContentAccess = false
                    blockNetworkLoads = true
                    domStorageEnabled = false
                    cacheMode = WebSettings.LOAD_NO_CACHE
                    setSupportMultipleWindows(false)
                    setGeolocationEnabled(false)
                }
                webViewClient = DiscWebViewClient(disc, { options })
            }
        },
        // Reload when the URL or the theme/font options change.
        update = { view ->
            view.webViewClient = DiscWebViewClient(disc, { options })
            if (view.url != url) view.loadUrl(url) else view.reload()
        },
    )
}
