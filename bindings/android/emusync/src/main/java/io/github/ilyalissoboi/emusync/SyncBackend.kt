package io.github.ilyalissoboi.emusync

import java.io.File

/**
 * The host's native emusync integration, which owns the emusync client (DESIGN §26).
 *
 * The emulator's C++ code creates the client with `es_client_create`, registers its cards and
 * calls the session hooks around its own card I/O, as PCSX2 and ARMSX2 do; this interface is the
 * small part of it the Kotlin side drives, usually over a few JNI functions of the host's. The
 * AAR's [SyncController] calls it, from its own background thread except where noted.
 */
interface SyncBackend {
    /** Whether the host's native code includes sync (a build without it can compile it out). */
    val isAvailable: Boolean

    /**
     * Starts sync with [config], or restarts it when it is running: the native side creates the
     * client in [SyncConfig.dataDir], adds the storage with the given keys, unlocks or creates
     * an encrypted repository when [SyncConfig.passphrase] is set, and registers its cards.
     * Blocks, for up to the ~10 s the first contact with the storage may take. Returns null when
     * sync runs, else a sentence for the user saying why not.
     */
    fun start(config: SyncConfig): String?

    /** Stops sync, finishing what it can in a short time. Does nothing when it isn't running. */
    fun stop()

    /** Whether sync is running. */
    val isRunning: Boolean

    /** Asks for a sync now (`es_sync_now`); does not wait for it. */
    fun syncNow()

    /** One line about the last sync for a settings screen; empty when nothing happened yet. */
    fun status(): String

    /**
     * The app went to the background (true) or came back (false). Called on the main thread, so
     * it must not block: the host flushes its cards and starts an upload in the background
     * (`es_app_suspend`) or catches up (`es_app_resume`) in whatever order its emulator needs.
     */
    fun setAppBackgrounded(backgrounded: Boolean)
}

/**
 * What [SyncBackend.start] needs, from [SyncSettings]. [toString] leaves out the secret access key,
 * the Dropbox refresh token and the passphrase, so a config can be logged.
 *
 * The host's credential provider answers with the S3 keys (`ES_CRED_S3`), or, for a `dropbox:`
 * storage ([isDropbox]), with [dropboxRefreshToken] as `ES_CRED_OAUTH_REFRESH` (DESIGN §30.5).
 */
class SyncConfig @JvmOverloads constructor(
    /** emusync's data folder for this storage: database, upload queue, repository key. */
    val dataDir: File,
    /** This device's name, as other devices show it. */
    val deviceName: String,
    /** The host app's version, recorded with each save version. */
    val appVersion: String,
    /** The storage, e.g. `s3://bucket/prefix?region=eu-west-1` or `dropbox:/saves`. */
    val remoteUri: String,
    /** S3 only; empty for Dropbox. */
    val accessKeyId: String,
    /** S3 only; empty for Dropbox. */
    val secretAccessKey: String,
    /** The end-to-end encryption passphrase; null when the repository isn't encrypted. */
    val passphrase: String?,
    /** Dropbox only: the sign-in's refresh token, from [DropboxSignIn]; null for S3. */
    val dropboxRefreshToken: String? = null,
) {
    /** The storage is Dropbox. */
    val isDropbox: Boolean get() = SyncSettings.isDropboxUri(remoteUri)

    override fun toString(): String =
        "SyncConfig(dataDir=$dataDir, deviceName=$deviceName, appVersion=$appVersion, " +
            "remoteUri=$remoteUri, accessKeyId=$accessKeyId, secretAccessKey=<hidden>, " +
            "passphrase=${if (passphrase == null) "none" else "<hidden>"}, " +
            "dropboxRefreshToken=${if (dropboxRefreshToken == null) "none" else "<hidden>"})"
}
