package io.github.ilyalissoboi.emusync

import android.os.Handler
import android.os.Looper
import android.util.Log
import androidx.annotation.MainThread
import androidx.annotation.WorkerThread
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors

/**
 * Starts and stops the host's sync ([backend]) to match the user's [settings], and tells it when
 * the app leaves the screen and comes back (DESIGN §26). Create one per app, for example in the
 * Application or the main activity's companion, and keep it.
 *
 * Starting can block for ~10 s (the first contact with the storage), so it runs on a thread that
 * belongs to the controller, not to a screen: a settings screen closing right after a change (a
 * field saved as it loses focus) must not cancel the restart, or the stored settings and the
 * running sync disagree. Starts run one at a time, in order, and each reads the settings when it
 * runs, so after several quick changes the last one wins.
 */
class SyncController(
    val settings: SyncSettings,
    val backend: SyncBackend,
    /** The host app's version, recorded with each save version. */
    private val appVersion: String,
) {
    private val worker: ExecutorService = Executors.newSingleThreadExecutor { Thread(it, "emusync-start") }
    private val mainHandler = Handler(Looper.getMainLooper())
    private var lifecycleObserver: DefaultLifecycleObserver? = null

    /** Whether the host's build includes sync. */
    val isAvailable: Boolean get() = runCatching { backend.isAvailable }.getOrDefault(false)

    /** Whether sync is running. */
    val isRunning: Boolean get() = isAvailable && runCatching { backend.isRunning }.getOrDefault(false)

    /**
     * Starts sync with the stored settings, or stops it when it is switched off or the settings
     * are incomplete. Blocks: never call it on the main thread. Returns null when sync is running
     * or deliberately off, else a sentence for the user.
     */
    @WorkerThread
    fun start(): String? {
        if (!isAvailable) return null
        val config = settings.config(appVersion)
        if (config == null) {
            runCatching { backend.stop() }
            return null
        }
        return runCatching { backend.start(config) }.getOrElse { e ->
            Log.w(TAG, "start failed", e)
            e.message ?: e.toString()
        }
    }

    /** [start] on the controller's thread, then [onDone] with its result on the main thread. */
    fun startAsync(onDone: (String?) -> Unit) {
        worker.execute {
            val result = start()
            mainHandler.post { onDone(result) }
        }
    }

    /** [start] in the background with nothing waiting for it, e.g. at app start. */
    fun startInBackground() = startAsync { error -> error?.let { Log.w(TAG, "not syncing: $it") } }

    /** Stops sync, after any start already queued. Does not wait. */
    fun stopAsync() {
        worker.execute { if (isAvailable) runCatching { backend.stop() } }
    }

    /** Asks for a sync now. */
    fun syncNow() {
        if (isAvailable) runCatching { backend.syncNow() }
    }

    /** One line about the last sync; empty when nothing happened yet or sync isn't available. */
    fun status(): String = if (isAvailable) runCatching { backend.status() }.getOrDefault("") else ""

    /**
     * Tells [backend] when the app as a whole goes to the background and comes back
     * ([ProcessLifecycleOwner]: its last activity stopped, or its first one started), so it can
     * upload new saves before Android may stop the process. Call once, on the main thread.
     */
    @MainThread
    fun attachToAppLifecycle() {
        if (lifecycleObserver != null) return
        val observer = object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) = setAppBackgrounded(false)
            override fun onStop(owner: LifecycleOwner) = setAppBackgrounded(true)
        }
        ProcessLifecycleOwner.get().lifecycle.addObserver(observer)
        lifecycleObserver = observer
    }

    /** Undoes [attachToAppLifecycle]. */
    @MainThread
    fun detachFromAppLifecycle() {
        lifecycleObserver?.let { ProcessLifecycleOwner.get().lifecycle.removeObserver(it) }
        lifecycleObserver = null
    }

    /**
     * For hosts that follow their own activity instead of [attachToAppLifecycle]: call from its
     * onStop (true) and onStart (false). Main thread.
     */
    @MainThread
    fun setAppBackgrounded(backgrounded: Boolean) {
        if (isAvailable) {
            runCatching { backend.setAppBackgrounded(backgrounded) }
                .onFailure { Log.w(TAG, "setAppBackgrounded($backgrounded) failed", it) }
        }
    }

    private companion object {
        const val TAG = "emusync"
    }
}
