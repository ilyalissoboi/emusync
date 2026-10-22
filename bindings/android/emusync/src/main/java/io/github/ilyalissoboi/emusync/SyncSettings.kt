package io.github.ilyalissoboi.emusync

import android.content.Context
import android.content.SharedPreferences
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import android.util.Log
import java.io.File
import java.security.KeyStore
import java.security.MessageDigest
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * The sync settings the user enters, kept the way ARMSX2 keeps them (DESIGN §26):
 *
 * - The storage address, access key ID, device name and the on/off switch are in
 *   SharedPreferences ([prefsName]).
 * - The secret access key and the optional encryption passphrase are encrypted with an AES key in
 *   the Android Keystore ([keyAlias]), which never leaves the device. Android's backup may copy
 *   the encrypted values to a new phone but not the key, so there they read as "not set" and the
 *   user enters them again.
 * - emusync's data for a storage lives in `noBackupFilesDir/emusync-<hash of the address>`
 *   ([dataDir]): it must not be restored onto another device, whose installation would then share
 *   this one's device identity. One folder per address, because emusync binds a data folder to
 *   one storage for good.
 *
 * The defaults suit a new host; ARMSX2's existing settings are `prefsName = "memcard_sync"`,
 * `keyAlias = "armsx2.memcardSync.secret"`. Thread-safe; the Keystore calls take a few
 * milliseconds.
 */
class SyncSettings @JvmOverloads constructor(
    context: Context,
    val prefsName: String = "emusync",
    val keyAlias: String = "emusync.settings.secret",
) {
    private val app = context.applicationContext

    data class Values(
        val enabled: Boolean,
        val remoteUri: String,
        val accessKeyId: String,
        /** A secret access key is stored and can be decrypted on this device. */
        val hasSecret: Boolean,
        /** An encryption passphrase is stored and can be decrypted on this device. */
        val hasPassphrase: Boolean,
        /** The stored name, or [defaultDeviceName] when none is. */
        val deviceName: String,
    ) {
        /** Switched on, with storage and both keys: what [SyncController] needs to start. */
        val isComplete: Boolean get() = enabled && remoteUri.isNotBlank() && accessKeyId.isNotBlank() && hasSecret
    }

    fun load(): Values {
        val p = prefs()
        return Values(
            enabled = p.getBoolean(KEY_ENABLED, false),
            remoteUri = p.getString(KEY_REMOTE, "").orEmpty(),
            accessKeyId = p.getString(KEY_ACCESS_KEY_ID, "").orEmpty(),
            hasSecret = unseal(KEY_SECRET) != null,
            hasPassphrase = unseal(KEY_PASSPHRASE) != null,
            deviceName = p.getString(KEY_DEVICE, "").orEmpty().ifBlank { defaultDeviceName() },
        )
    }

    fun setEnabled(enabled: Boolean) = edit { putBoolean(KEY_ENABLED, enabled) }
    fun setRemoteUri(value: String) = edit { putString(KEY_REMOTE, value.trim()) }
    fun setAccessKeyId(value: String) = edit { putString(KEY_ACCESS_KEY_ID, value.trim()) }

    /** Stores the device name; an empty value goes back to [defaultDeviceName]. */
    fun setDeviceName(value: String) = edit { putString(KEY_DEVICE, value.trim()) }

    /** Stores the secret access key encrypted; an empty value removes it. */
    fun setSecretAccessKey(value: String) = seal(KEY_SECRET, value.trim())

    /**
     * Stores the end-to-end encryption passphrase encrypted; an empty value removes it. Not
     * trimmed: spaces can be part of a passphrase.
     */
    fun setPassphrase(value: String) = seal(KEY_PASSPHRASE, value)

    /**
     * The stored passphrase, for a settings screen's Show: the user needs it again to set up their
     * other devices, and losing it loses the saves. Null when none is stored or it can't be
     * decrypted here. The secret access key has no getter: it can be issued again.
     */
    fun passphrase(): String? = unseal(KEY_PASSPHRASE)

    /** emusync's data folder for [remoteUri], created if needed; see the class comment. */
    fun dataDir(remoteUri: String): File {
        val digest = MessageDigest.getInstance("SHA-256").digest(remoteUri.toByteArray(Charsets.UTF_8))
        val id = digest.take(8).joinToString("") { "%02x".format(it) }
        return File(app.noBackupFilesDir, "emusync-$id").apply { mkdirs() }
    }

    /** The config to start sync with, or null when sync is off or the settings are incomplete. */
    fun config(appVersion: String): SyncConfig? {
        val v = load()
        if (!v.enabled || v.remoteUri.isBlank() || v.accessKeyId.isBlank()) return null
        val secret = unseal(KEY_SECRET) ?: return null
        return SyncConfig(
            dataDir = dataDir(v.remoteUri),
            deviceName = v.deviceName,
            appVersion = appVersion,
            remoteUri = v.remoteUri,
            accessKeyId = v.accessKeyId,
            secretAccessKey = secret,
            passphrase = unseal(KEY_PASSPHRASE),
        )
    }

    // ---- storage ---------------------------------------------------------------------------

    private fun prefs(): SharedPreferences = app.getSharedPreferences(prefsName, Context.MODE_PRIVATE)

    private inline fun edit(block: SharedPreferences.Editor.() -> Unit) {
        prefs().edit().apply(block).apply()
    }

    private fun seal(key: String, value: String) {
        if (value.isEmpty()) {
            edit { remove(key) }
            return
        }
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, keystoreKey())
        val sealed = cipher.iv + cipher.doFinal(value.toByteArray(Charsets.UTF_8))
        edit { putString(key, Base64.encodeToString(sealed, Base64.NO_WRAP)) }
    }

    private fun unseal(key: String): String? {
        val sealed = prefs().getString(key, null) ?: return null
        return runCatching {
            val bytes = Base64.decode(sealed, Base64.NO_WRAP)
            val cipher = Cipher.getInstance(TRANSFORMATION)
            // GCM's IV is 12 bytes; the tag is the last 16 of the ciphertext.
            cipher.init(Cipher.DECRYPT_MODE, existingKey() ?: return null, GCMParameterSpec(128, bytes, 0, 12))
            String(cipher.doFinal(bytes, 12, bytes.size - 12), Charsets.UTF_8)
        }.getOrElse {
            // A value restored from another device's backup, or a key the system wiped: unreadable
            // here, so ask for it again rather than fail every start.
            Log.w(TAG, "stored $key cannot be decrypted on this device", it)
            null
        }
    }

    private fun existingKey(): SecretKey? {
        val ks = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        return (ks.getEntry(keyAlias, null) as? KeyStore.SecretKeyEntry)?.secretKey
    }

    private fun keystoreKey(): SecretKey = synchronized(keyLock) {
        existingKey() ?: KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE).apply {
            init(
                KeyGenParameterSpec.Builder(keyAlias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setKeySize(256)
                    .build(),
            )
        }.generateKey()
    }

    companion object {
        private const val TAG = "emusync"
        private const val KEY_ENABLED = "enabled"
        private const val KEY_REMOTE = "remoteUri"
        private const val KEY_ACCESS_KEY_ID = "accessKeyId"
        private const val KEY_SECRET = "secretAccessKey.enc"
        private const val KEY_PASSPHRASE = "passphrase.enc"
        private const val KEY_DEVICE = "deviceName"
        private const val KEYSTORE = "AndroidKeyStore"
        private const val TRANSFORMATION = "AES/GCM/NoPadding"

        /** Two settings objects creating the same key at once must not make two. */
        private val keyLock = Any()

        /** "Manufacturer Model", e.g. "AYN Thor"; "Android" when the system reports neither. */
        @JvmStatic
        fun defaultDeviceName(): String =
            listOf(Build.MANUFACTURER, Build.MODEL).filter { it.isNotBlank() }.distinct().joinToString(" ")
                .ifBlank { "Android" }
    }
}
