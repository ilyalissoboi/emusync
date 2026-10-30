package io.github.ilyalissoboi.emusync

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.util.Base64
import android.util.Log
import androidx.annotation.WorkerThread
import androidx.browser.customtabs.CustomTabsIntent
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.net.URLEncoder
import java.security.MessageDigest
import java.security.SecureRandom
import org.json.JSONObject

/**
 * Signing in to Dropbox (DESIGN §30.4, §30.6): the OAuth code flow with PKCE, in a Custom Tab, with
 * no client secret. The result, a refresh token, goes into [SyncSettings] (encrypted with the
 * Keystore key), and [SyncSettings.config] hands it to the host's native code.
 *
 * A settings screen:
 * 1. calls [begin] from its activity when the user taps "Sign in with Dropbox": the sign-in page
 *    opens in a Custom Tab;
 * 2. Dropbox redirects to [redirectUri], which [DropboxRedirectActivity] (declared by the AAR)
 *    receives: it keeps the redirect and brings the activity that called [begin] back to the front;
 * 3. in that activity's `onResume`, when [hasPendingRedirect], calls [completeAsync], and shows its
 *    result.
 *
 * Every step survives the app's process being killed while the browser is open: what [begin]
 * prepared (the PKCE verifier, encrypted) and the redirect are kept in preferences.
 *
 * [redirectUri] is `<applicationId>.emusync://dropbox`, one per app, so two emulators with the AAR
 * don't both claim the redirect. It must be registered with the Dropbox app; an application id with
 * capital letters isn't supported (URI schemes are lower-case).
 */
class DropboxSignIn internal constructor(
    context: Context,
    private val settings: SyncSettings,
    private val endpoints: Endpoints,
) {
    constructor(context: Context, settings: SyncSettings) : this(context, settings, Endpoints.DROPBOX)

    private val app = context.applicationContext

    /** Where Dropbox's API and sign-in page are. Tests point [api] at a fake server. */
    internal data class Endpoints(val api: String, val authorize: String) {
        companion object {
            val DROPBOX = Endpoints("https://api.dropboxapi.com", "https://www.dropbox.com/oauth2/authorize")
        }
    }

    /** How a sign-in ended. */
    sealed class Result {
        /** Signed in; the refresh token is stored. [email] may be empty. */
        data class SignedIn(val accountId: String, val email: String) : Result()

        /** Not signed in: [message] is a sentence for the user. */
        data class Failed(val message: String) : Result()
    }

    /** This app's redirect URI, to register with the Dropbox app. */
    val redirectUri: String get() = "${app.packageName}.emusync://dropbox"

    /**
     * Opens the Dropbox sign-in page for [remoteUri] (`dropbox:/saves`) in a Custom Tab, from
     * [activity], which the redirect brings back. Returns null when the page opened, else a
     * sentence for the user (no app key in this build, no browser).
     */
    fun begin(activity: Activity, remoteUri: String): String? {
        val url = try {
            prepare(remoteUri, activity.componentName)
        } catch (e: IllegalArgumentException) {
            return e.message
        }
        return try {
            CustomTabsIntent.Builder().build().launchUrl(activity, Uri.parse(url))
            null
        } catch (e: ActivityNotFoundException) {
            Log.w(TAG, "no browser for the Dropbox sign-in", e)
            "No web browser is installed to sign in to Dropbox with."
        }
    }

    /**
     * Prepares a sign-in and returns the URL of Dropbox's sign-in page. [returnTo] is the activity
     * the redirect brings back. Throws [IllegalArgumentException] with a sentence for the user.
     */
    internal fun prepare(remoteUri: String, returnTo: ComponentName?): String {
        require(SyncSettings.isDropboxUri(remoteUri)) { "Not a Dropbox address: $remoteUri" }
        val appKey = appKeyFor(remoteUri)
            ?: throw IllegalArgumentException(
                "This build has no Dropbox app key: add ?app_key=<key> to the address (DESIGN §30.1).",
            )
        val verifier = randomB64(32)
        val state = randomB64(16)
        val pending = JSONObject()
            .put("appKey", appKey)
            .put("verifier", verifier)
            .put("state", state)
            .put("redirectUri", redirectUri)
        settings.seal(KEY_PENDING, pending.toString())
        flow().edit()
            .remove(KEY_REDIRECT)
            .putString(KEY_RETURN_TO, returnTo?.flattenToString())
            .commit()
        return endpoints.authorize + "?" + form(
            "client_id" to appKey,
            "response_type" to "code",
            "code_challenge" to challenge(verifier),
            "code_challenge_method" to "S256",
            "token_access_type" to "offline",
            "state" to state,
            "redirect_uri" to redirectUri,
        )
    }

    /** A redirect arrived and [complete] hasn't handled it yet. */
    fun hasPendingRedirect(): Boolean = flow().getString(KEY_REDIRECT, null) != null

    /**
     * Finishes the sign-in with the redirect [DropboxRedirectActivity] kept: checks it belongs to
     * the sign-in [begin] started, exchanges the code for a refresh token and stores it with the
     * account. Blocks on the network: never call it on the main thread.
     */
    @WorkerThread
    fun complete(): Result {
        val redirect = flow().getString(KEY_REDIRECT, null)
            ?: return Result.Failed("The Dropbox sign-in didn't come back. Try again.")
        flow().edit().remove(KEY_REDIRECT).commit()
        val pending = settings.unseal(KEY_PENDING)?.let { runCatching { JSONObject(it) }.getOrNull() }
            ?: return Result.Failed("No Dropbox sign-in was started on this device. Try again.")
        val uri = Uri.parse(redirect)
        uri.getQueryParameter("error")?.let { error ->
            settings.seal(KEY_PENDING, "")
            return Result.Failed(
                if (error == "access_denied") "The Dropbox sign-in was cancelled."
                else "Dropbox didn't sign in: ${uri.getQueryParameter("error_description") ?: error}",
            )
        }
        if (uri.getQueryParameter("state") != pending.getString("state")) {
            return Result.Failed("That Dropbox sign-in belongs to another attempt. Try again.")
        }
        val code = uri.getQueryParameter("code")?.takeIf { it.isNotEmpty() }
            ?: return Result.Failed("Dropbox sent no sign-in code. Try again.")
        settings.seal(KEY_PENDING, "")
        return try {
            val token = post(
                "${endpoints.api}/oauth2/token",
                form(
                    "grant_type" to "authorization_code",
                    "code" to code,
                    "code_verifier" to pending.getString("verifier"),
                    "client_id" to pending.getString("appKey"),
                    "redirect_uri" to pending.getString("redirectUri"),
                ),
                "application/x-www-form-urlencoded",
                bearer = null,
            )
            val refresh = token.optString("refresh_token")
            if (refresh.isEmpty()) return Result.Failed("Dropbox sent no sign-in token. Try again.")
            var accountId = token.optString("account_id")
            // The email address is only for display: a failed lookup doesn't fail the sign-in.
            val email = runCatching {
                val account = post(
                    "${endpoints.api}/2/users/get_current_account",
                    "null",
                    "application/json",
                    bearer = token.getString("access_token"),
                )
                if (accountId.isEmpty()) accountId = account.optString("account_id")
                account.optString("email")
            }.onFailure { Log.w(TAG, "Dropbox account lookup failed", it) }.getOrDefault("")
            settings.setDropboxSignIn(refresh, accountId, email)
            Result.SignedIn(accountId, email)
        } catch (e: DropboxRefused) {
            Result.Failed("Dropbox refused the sign-in: ${e.message}")
        } catch (e: IOException) {
            Log.w(TAG, "Dropbox sign-in failed", e)
            Result.Failed("Dropbox can't be reached: ${e.message ?: e}")
        }
    }

    /** [complete] on a background thread, then [onDone] with its result on the main thread. */
    fun completeAsync(onDone: (Result) -> Unit) {
        val main = Handler(Looper.getMainLooper())
        Thread({ complete().let { main.post { onDone(it) } } }, "emusync-dropbox").start()
    }

    /**
     * Signs out: tells Dropbox to revoke the sign-in, then forgets it on this device whatever
     * Dropbox said. Blocks on the network: never call it on the main thread. Returns null, or a
     * sentence for the user when Dropbox couldn't be told (the sign-in is forgotten here anyway).
     */
    @WorkerThread
    fun signOut(): String? {
        val refresh = settings.dropboxRefreshToken()
        val appKey = appKeyFor(settings.load().remoteUri)
        val result = if (refresh == null || appKey == null) {
            null
        } else {
            try {
                val access = post(
                    "${endpoints.api}/oauth2/token",
                    form("grant_type" to "refresh_token", "refresh_token" to refresh, "client_id" to appKey),
                    "application/x-www-form-urlencoded",
                    bearer = null,
                ).getString("access_token")
                post("${endpoints.api}/2/auth/token/revoke", "null", "application/json", bearer = access)
                null
            } catch (e: DropboxRefused) {
                null // already revoked
            } catch (e: IOException) {
                "Dropbox couldn't be told (${e.message ?: e}); the sign-in is forgotten on this device."
            }
        }
        settings.clearDropboxSignIn()
        return result
    }

    /** The Dropbox app key for [remoteUri]: its `app_key`, else this build's. */
    internal fun appKeyFor(remoteUri: String): String? {
        val query = remoteUri.substringAfter('?', "")
        val param = query.split('&').firstNotNullOfOrNull { it.removePrefix("app_key=").takeIf { v -> v != it } }
        return param?.takeIf { it.isNotEmpty() } ?: BuildConfig.DROPBOX_APP_KEY.takeIf { it.isNotEmpty() }
    }

    private fun flow() = app.getSharedPreferences(FLOW_PREFS, Context.MODE_PRIVATE)

    /** Dropbox answered with a client error (a refused grant): [message] is its description. */
    private class DropboxRefused(message: String) : IOException(message)

    /** POSTs [body] and returns the JSON answer; [DropboxRefused] for a 4xx. */
    private fun post(url: String, body: String, contentType: String, bearer: String?): JSONObject {
        val conn = URL(url).openConnection() as HttpURLConnection
        try {
            conn.requestMethod = "POST"
            conn.connectTimeout = 10_000
            conn.readTimeout = 30_000
            conn.doOutput = true
            conn.setRequestProperty("Content-Type", contentType)
            bearer?.let { conn.setRequestProperty("Authorization", "Bearer $it") }
            conn.outputStream.use { it.write(body.toByteArray(Charsets.UTF_8)) }
            val status = conn.responseCode
            val text = (if (status < 400) conn.inputStream else conn.errorStream)
                ?.use { it.readBytes().toString(Charsets.UTF_8) }.orEmpty()
            val json = runCatching { JSONObject(text.ifBlank { "{}" }) }.getOrNull()
            return when (status) {
                in 200..299 -> json ?: JSONObject()
                in 400..499 -> throw DropboxRefused(
                    json?.optString("error_description")?.takeIf { it.isNotEmpty() }
                        ?: json?.optString("error_summary")?.takeIf { it.isNotEmpty() }
                        ?: json?.optString("error")?.takeIf { it.isNotEmpty() }
                        ?: "HTTP $status",
                )
                else -> throw IOException("HTTP $status")
            }
        } finally {
            conn.disconnect()
        }
    }

    companion object {
        private const val TAG = "emusync"

        /** The sign-in's state the redirect activity shares: no secrets. */
        internal const val FLOW_PREFS = "emusync.dropbox.signin"
        internal const val KEY_REDIRECT = "redirect"
        internal const val KEY_RETURN_TO = "returnTo"

        /** What [prepare] keeps until [complete], encrypted in the host's [SyncSettings]. */
        private const val KEY_PENDING = "dropboxPending.enc"

        private val random = SecureRandom()

        private fun randomB64(n: Int): String =
            ByteArray(n).also(random::nextBytes).let { Base64.encodeToString(it, B64) }

        /** The PKCE code challenge for [verifier] (S256). */
        internal fun challenge(verifier: String): String =
            Base64.encodeToString(MessageDigest.getInstance("SHA-256").digest(verifier.toByteArray()), B64)

        private const val B64 = Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP

        private fun form(vararg pairs: Pair<String, String>): String =
            pairs.joinToString("&") { (k, v) -> "${URLEncoder.encode(k, "UTF-8")}=${URLEncoder.encode(v, "UTF-8")}" }

        /**
         * Keeps a redirect [DropboxRedirectActivity] received and returns the intent that brings
         * the activity that began the sign-in back, over the Custom Tab; null when it isn't known.
         */
        internal fun onRedirect(context: Context, redirect: Uri): Intent? {
            val prefs = context.getSharedPreferences(FLOW_PREFS, Context.MODE_PRIVATE)
            prefs.edit().putString(KEY_REDIRECT, redirect.toString()).commit()
            val component = prefs.getString(KEY_RETURN_TO, null)?.let(ComponentName::unflattenFromString)
                ?: return context.packageManager.getLaunchIntentForPackage(context.packageName)
            return Intent().setComponent(component)
                .addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        }
    }
}
