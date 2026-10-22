package io.github.ilyalissoboi.emusync

import android.content.Context
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.security.KeyStore
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
class SyncSettingsTest {
    private val context: Context = InstrumentationRegistry.getInstrumentation().targetContext
    private val prefsName = "emusync-test-settings"
    private val keyAlias = "emusync.test.settings"
    private lateinit var settings: SyncSettings

    @Before
    fun setUp() {
        cleanUp()
        settings = SyncSettings(context, prefsName, keyAlias)
    }

    @After
    fun cleanUp() {
        // The context keeps handing out its cached instance after the file is deleted, with the old
        // values: clear that too, synchronously.
        context.getSharedPreferences(prefsName, Context.MODE_PRIVATE).edit().clear().commit()
        context.deleteSharedPreferences(prefsName)
        KeyStore.getInstance("AndroidKeyStore").apply { load(null) }.deleteEntry(keyAlias)
    }

    private fun fill() {
        settings.setEnabled(true)
        settings.setRemoteUri("  s3://saves/emusync/v1?region=eu-west-1 ")
        settings.setAccessKeyId(" AKIDEXAMPLE ")
        settings.setSecretAccessKey(" secret/key ")
    }

    @Test
    fun startsEmptyAndOff() {
        val v = settings.load()
        assertFalse(v.enabled)
        assertEquals("", v.remoteUri)
        assertFalse(v.hasSecret)
        assertFalse(v.hasPassphrase)
        assertEquals(SyncSettings.defaultDeviceName(), v.deviceName)
        assertFalse(v.isComplete)
        assertNull(settings.config("1.0"))
    }

    @Test
    fun configHasTheTrimmedValuesAndSecrets() {
        fill()
        settings.setDeviceName(" Thor ")
        settings.setPassphrase("  a passphrase with spaces ")
        val v = settings.load()
        assertTrue(v.isComplete)
        assertTrue(v.hasPassphrase)
        assertEquals("Thor", v.deviceName)
        val c = settings.config("2.0")
        assertNotNull(c)
        c!!
        assertEquals("s3://saves/emusync/v1?region=eu-west-1", c.remoteUri)
        assertEquals("AKIDEXAMPLE", c.accessKeyId)
        assertEquals("secret/key", c.secretAccessKey)
        assertEquals("  a passphrase with spaces ", c.passphrase)
        assertEquals("  a passphrase with spaces ", settings.passphrase())
        assertEquals("secret/key", settings.secretAccessKey())
        assertEquals("2.0", c.appVersion)
        assertEquals("Thor", c.deviceName)
        assertEquals(settings.dataDir(c.remoteUri), c.dataDir)
        // A config can be logged: no secrets in it.
        assertFalse(c.toString().contains("secret/key"))
        assertFalse(c.toString().contains("passphrase with spaces"))
    }

    @Test
    fun secretsAreNotStoredInTheClear() {
        fill()
        settings.setPassphrase("hunter2 hunter2")
        val stored = context.getSharedPreferences(prefsName, Context.MODE_PRIVATE).all.values.joinToString()
        assertFalse(stored.contains("secret/key"))
        assertFalse(stored.contains("hunter2"))
    }

    @Test
    fun emptyValuesRemoveSecretsAndIncompleteSettingsGiveNoConfig() {
        fill()
        settings.setPassphrase("p")
        settings.setPassphrase("")
        assertNull(settings.passphrase())
        assertNull(settings.config("1")!!.passphrase)
        settings.setSecretAccessKey("")
        assertFalse(settings.load().hasSecret)
        assertNull(settings.secretAccessKey())
        assertNull(settings.config("1"))
        fill()
        settings.setEnabled(false)
        assertNull(settings.config("1"))
    }

    /** A value restored from another device's backup, whose Keystore key isn't here. */
    @Test
    fun secretsWithoutTheirKeyReadAsNotSet() {
        fill()
        KeyStore.getInstance("AndroidKeyStore").apply { load(null) }.deleteEntry(keyAlias)
        assertFalse(settings.load().hasSecret)
        assertNull(settings.config("1"))
        // Entering it again works, with a new key.
        settings.setSecretAccessKey("again")
        assertEquals("again", settings.config("1")!!.secretAccessKey)
    }

    @Test
    fun oneDataFolderPerStorageOutsideBackups() {
        val a = settings.dataDir("s3://a/emusync")
        assertEquals(a, settings.dataDir("s3://a/emusync"))
        assertNotEquals(a, settings.dataDir("s3://b/emusync"))
        assertTrue(a.isDirectory)
        assertEquals(context.noBackupFilesDir, a.parentFile)
        assertTrue(a.name.matches(Regex("emusync-[0-9a-f]{16}")))
    }
}
