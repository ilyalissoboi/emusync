package io.github.ilyalissoboi.emusync

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.net.ServerSocket
import java.net.URLDecoder
import java.security.KeyStore
import java.util.Collections
import kotlin.concurrent.thread
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class DropboxSignInTest {
    private val context: Context = InstrumentationRegistry.getInstrumentation().targetContext
    private val prefsName = "emusync-test-dropbox"
    private val keyAlias = "emusync.test.dropbox"
    private val uri = "dropbox:/saves?app_key=testkey"
    private lateinit var settings: SyncSettings
    private lateinit var fake: FakeDropbox
    private lateinit var signIn: DropboxSignIn

    /** Dropbox's token endpoint, account lookup and revoke, on 127.0.0.1. */
    private class FakeDropbox : AutoCloseable {
        private val server = ServerSocket(0)
        val base = "http://127.0.0.1:${server.localPort}"

        /** Codes waiting to be exchanged → their PKCE challenge. */
        val codes: MutableMap<String, String> = Collections.synchronizedMap(mutableMapOf())
        val refreshTokens: MutableSet<String> = Collections.synchronizedSet(mutableSetOf())
        val requests: MutableList<String> = Collections.synchronizedList(mutableListOf())

        init {
            thread(isDaemon = true) {
                while (!server.isClosed) {
                    val socket = runCatching { server.accept() }.getOrNull() ?: break
                    socket.use { s ->
                        val input = s.getInputStream().bufferedReader()
                        val path = input.readLine()?.split(' ')?.getOrNull(1) ?: return@use
                        var length = 0
                        while (true) {
                            val h = input.readLine() ?: break
                            if (h.isEmpty()) break
                            if (h.startsWith("Content-Length:", ignoreCase = true)) length = h.substringAfter(':').trim().toInt()
                        }
                        val body = CharArray(length).also { input.read(it, 0, length) }.concatToString()
                        requests += path
                        val (status, json) = handle(path, body)
                        val out = json.toString().toByteArray()
                        s.getOutputStream().apply {
                            write("HTTP/1.1 $status X\r\nContent-Type: application/json\r\nContent-Length: ${out.size}\r\nConnection: close\r\n\r\n".toByteArray())
                            write(out)
                            flush()
                        }
                    }
                }
            }
        }

        private fun handle(path: String, body: String): Pair<Int, JSONObject> {
            val form = body.split('&').filter { '=' in it }
                .associate { URLDecoder.decode(it.substringBefore('='), "UTF-8") to URLDecoder.decode(it.substringAfter('='), "UTF-8") }
            val refused = 400 to JSONObject().put("error", "invalid_grant").put("error_description", "code doesn't exist or has expired")
            return when (path) {
                "/oauth2/token" -> when {
                    form["client_id"] != "testkey" -> 400 to JSONObject().put("error", "invalid_client")
                    form["grant_type"] == "authorization_code" -> {
                        val challenge = codes.remove(form["code"]) ?: return refused
                        if (DropboxSignIn.challenge(form["code_verifier"].orEmpty()) != challenge) return refused
                        refreshTokens += "refresh-1"
                        200 to JSONObject().put("access_token", "access-1").put("refresh_token", "refresh-1")
                            .put("account_id", "dbid:fake").put("expires_in", 14400)
                    }
                    form["grant_type"] == "refresh_token" && form["refresh_token"] in refreshTokens ->
                        200 to JSONObject().put("access_token", "access-2").put("expires_in", 14400)
                    else -> refused
                }
                "/2/users/get_current_account" -> 200 to JSONObject().put("account_id", "dbid:fake").put("email", "tester@example.com")
                "/2/auth/token/revoke" -> {
                    refreshTokens.clear()
                    200 to JSONObject()
                }
                else -> 404 to JSONObject()
            }
        }

        override fun close() = server.close()
    }

    @Before
    fun setUp() {
        cleanUp()
        settings = SyncSettings(context, prefsName, keyAlias)
        fake = FakeDropbox()
        signIn = DropboxSignIn(context, settings, DropboxSignIn.Endpoints(fake.base, "https://www.dropbox.com/oauth2/authorize"))
    }

    @After
    fun cleanUp() {
        if (::fake.isInitialized) fake.close()
        for (name in listOf(prefsName, DropboxSignIn.FLOW_PREFS)) {
            context.getSharedPreferences(name, Context.MODE_PRIVATE).edit().clear().commit()
            context.deleteSharedPreferences(name)
        }
        KeyStore.getInstance("AndroidKeyStore").apply { load(null) }.deleteEntry(keyAlias)
    }

    /** What Dropbox does once the user approves: a code for this sign-in's challenge, in a redirect. */
    private fun approve(url: String): Uri {
        val page = Uri.parse(url)
        val code = "code-${fake.codes.size + 1}"
        fake.codes[code] = page.getQueryParameter("code_challenge")!!
        return Uri.parse("${signIn.redirectUri}?code=$code&state=${page.getQueryParameter("state")}")
    }

    @Test
    fun theSignInPageAsksForAnOfflineTokenWithPkce() {
        val page = Uri.parse(signIn.prepare(uri, null))
        assertEquals("www.dropbox.com", page.host)
        assertEquals("testkey", page.getQueryParameter("client_id"))
        assertEquals("code", page.getQueryParameter("response_type"))
        assertEquals("S256", page.getQueryParameter("code_challenge_method"))
        assertEquals("offline", page.getQueryParameter("token_access_type"))
        assertEquals("${context.packageName}.emusync://dropbox", page.getQueryParameter("redirect_uri"))
        assertEquals(43, page.getQueryParameter("code_challenge")!!.length)
        // Another sign-in, another state.
        assertNotEquals(page.getQueryParameter("state"), Uri.parse(signIn.prepare(uri, null)).getQueryParameter("state"))
    }

    @Test
    fun signInStoresTheTokenAndAccount() {
        val redirect = approve(signIn.prepare(uri, null))
        assertFalse(signIn.hasPendingRedirect())
        DropboxSignIn.onRedirect(context, redirect)
        assertTrue(signIn.hasPendingRedirect())

        val result = signIn.complete()
        assertEquals(DropboxSignIn.Result.SignedIn("dbid:fake", "tester@example.com"), result)
        assertFalse(signIn.hasPendingRedirect())
        assertEquals("refresh-1", settings.dropboxRefreshToken())
        assertEquals("dbid:fake", settings.dropboxAccountId())

        settings.setEnabled(true)
        settings.setRemoteUri(uri)
        val v = settings.load()
        assertTrue(v.isDropbox && v.hasDropboxSignIn && v.isComplete)
        assertEquals("tester@example.com", v.dropboxEmail)
        val config = settings.config("1.0")!!
        assertTrue(config.isDropbox)
        assertEquals("refresh-1", config.dropboxRefreshToken)
        assertEquals("", config.accessKeyId)
        assertFalse(config.toString().contains("refresh-1"))
        // Another account gets another data folder; the address alone keeps S3's naming.
        assertEquals(settings.dataDir(uri, "dbid:fake"), config.dataDir)
        assertNotEquals(settings.dataDir(uri), config.dataDir)
        assertNotEquals(settings.dataDir(uri, "dbid:other"), config.dataDir)

        // A redirect replayed later finds no sign-in waiting.
        DropboxSignIn.onRedirect(context, redirect)
        assertTrue(signIn.complete() is DropboxSignIn.Result.Failed)
    }

    @Test
    fun redirectsThatDontMatchAreRefused() {
        val url = signIn.prepare(uri, null)
        val good = approve(url)
        // Another attempt's state.
        DropboxSignIn.onRedirect(context, Uri.parse("${signIn.redirectUri}?code=x&state=someone-else"))
        assertTrue(signIn.complete() is DropboxSignIn.Result.Failed)
        assertNull(settings.dropboxRefreshToken())
        // A code that doesn't match the verifier (exchanged by another sign-in).
        val other = Uri.parse(signIn.prepare(uri, null))
        val state = other.getQueryParameter("state")
        DropboxSignIn.onRedirect(context, Uri.parse("${signIn.redirectUri}?code=${good.getQueryParameter("code")}&state=$state"))
        val refused = signIn.complete()
        assertTrue(refused is DropboxSignIn.Result.Failed && refused.message.startsWith("Dropbox refused"))
        // The user cancelled.
        signIn.prepare(uri, null)
        DropboxSignIn.onRedirect(context, Uri.parse("${signIn.redirectUri}?error=access_denied"))
        assertEquals(DropboxSignIn.Result.Failed("The Dropbox sign-in was cancelled."), signIn.complete())
        // Nothing came back at all.
        assertTrue(signIn.complete() is DropboxSignIn.Result.Failed)
        assertNull(settings.dropboxRefreshToken())
    }

    @Test
    fun unreachableDropboxIsReported() {
        val redirect = approve(signIn.prepare(uri, null))
        fake.close()
        DropboxSignIn.onRedirect(context, redirect)
        val r = signIn.complete()
        assertTrue(r is DropboxSignIn.Result.Failed && r.message.startsWith("Dropbox can't be reached"))
    }

    @Test
    fun theRedirectBringsBackTheActivityThatBegan() {
        val component = ComponentName(context, "com.example.Settings")
        signIn.prepare(uri, component)
        val back = DropboxSignIn.onRedirect(context, Uri.parse("${signIn.redirectUri}?code=c&state=s"))!!
        assertEquals(component, back.component)
        assertTrue(back.flags and Intent.FLAG_ACTIVITY_CLEAR_TOP != 0)
    }

    @Test
    fun appKeysAndAddresses() {
        assertEquals("testkey", signIn.appKeyFor(uri))
        assertEquals("k2", signIn.appKeyFor("dropbox:/s?x=1&app_key=k2"))
        if (BuildConfig.DROPBOX_APP_KEY.isEmpty()) {
            assertNull(signIn.appKeyFor("dropbox:/saves"))
            assertTrue(runCatching { signIn.prepare("dropbox:/saves", null) }.exceptionOrNull() is IllegalArgumentException)
        }
        assertTrue(runCatching { signIn.prepare("s3://b/p", null) }.exceptionOrNull() is IllegalArgumentException)
        assertTrue(SyncSettings.isDropboxUri(" dropbox:/saves"))
        assertFalse(SyncSettings.isDropboxUri("s3://b/p"))
    }

    @Test
    fun signOutRevokesAndForgets() {
        settings.setRemoteUri(uri)
        DropboxSignIn.onRedirect(context, approve(signIn.prepare(uri, null)))
        assertTrue(signIn.complete() is DropboxSignIn.Result.SignedIn)
        assertNull(signIn.signOut())
        assertTrue("/2/auth/token/revoke" in fake.requests)
        assertTrue(fake.refreshTokens.isEmpty())
        assertNull(settings.dropboxRefreshToken())
        assertEquals("", settings.dropboxAccountId())
        assertFalse(settings.load().hasDropboxSignIn)
        // Signing out again, or offline, still forgets.
        assertNull(signIn.signOut())
        settings.setDropboxSignIn("refresh-x", "dbid:x", "")
        fake.close()
        assertNotNull(signIn.signOut())
        assertNull(settings.dropboxRefreshToken())
    }
}
