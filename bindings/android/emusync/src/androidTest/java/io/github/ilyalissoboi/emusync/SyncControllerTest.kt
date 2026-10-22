package io.github.ilyalissoboi.emusync

import android.content.Context
import android.os.Looper
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.security.KeyStore
import java.util.Collections
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class SyncControllerTest {
    /** Records what the controller asks of the host. */
    private class FakeBackend(override val isAvailable: Boolean = true) : SyncBackend {
        val calls: MutableList<String> = Collections.synchronizedList(mutableListOf())
        var started: SyncConfig? = null
        var startResult: String? = null
        var startError: Exception? = null
        var stopError: Exception? = null
        /** Counted down by every stop, for tests that queue one with stopAsync(), which has no callback. */
        val stopped = CountDownLatch(1)
        @Volatile var running = false

        override fun start(config: SyncConfig): String? {
            calls += "start ${config.remoteUri}"
            startError?.let { throw it }
            started = config
            running = startResult == null
            return startResult
        }
        override fun stop() {
            calls += "stop"
            stopped.countDown()
            stopError?.let { throw it }
            running = false
        }
        override val isRunning get() = running
        override fun syncNow() { calls += "syncNow" }
        override fun status() = "Synced just now"
        override fun setAppBackgrounded(backgrounded: Boolean) {
            assertTrue("called on the main thread", Looper.myLooper() == Looper.getMainLooper())
            calls += "backgrounded $backgrounded"
        }
    }

    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context: Context = instrumentation.targetContext
    private val prefsName = "emusync-test-controller"
    private val keyAlias = "emusync.test.controller"
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

    private fun complete(remote: String = "s3://saves/emusync") {
        settings.setEnabled(true)
        settings.setRemoteUri(remote)
        settings.setAccessKeyId("AKID")
        settings.setSecretAccessKey("secret")
    }

    @Test
    fun incompleteSettingsStopInsteadOfStarting() {
        val backend = FakeBackend()
        val controller = SyncController(settings, backend, "1.0")
        settings.setEnabled(true)
        assertNull(controller.start())
        assertEquals(listOf("stop"), backend.calls)
    }

    /** Switched off while running, and the native side can't stop: the caller must hear of it. */
    @Test
    fun aFailedStopIsReported() {
        val backend = FakeBackend().apply { stopError = IllegalStateException("worker stuck") }
        val controller = SyncController(settings, backend, "1.0")
        complete()
        assertNull(controller.start())
        settings.setEnabled(false)
        assertEquals("Sync could not be stopped: worker stuck", controller.start())
        assertTrue(controller.isRunning)
    }

    @Test
    fun completeSettingsStartWithTheirConfig() {
        val backend = FakeBackend()
        val controller = SyncController(settings, backend, "3.1")
        complete()
        assertNull(controller.start())
        assertTrue(controller.isRunning)
        val c = backend.started!!
        assertEquals("s3://saves/emusync", c.remoteUri)
        assertEquals("secret", c.secretAccessKey)
        assertEquals("3.1", c.appVersion)
        assertEquals(settings.dataDir(c.remoteUri), c.dataDir)
        assertEquals("Synced just now", controller.status())
    }

    @Test
    fun startProblemsComeBackAsASentence() {
        val backend = FakeBackend().apply { startResult = "Wrong passphrase" }
        val controller = SyncController(settings, backend, "1.0")
        complete()
        assertEquals("Wrong passphrase", controller.start())
        backend.startError = IllegalStateException("no network")
        assertEquals("no network", controller.start())
    }

    @Test
    fun aBuildWithoutSyncDoesNothing() {
        val backend = FakeBackend(isAvailable = false)
        val controller = SyncController(settings, backend, "1.0")
        complete()
        assertNull(controller.start())
        controller.syncNow()
        instrumentation.runOnMainSync { controller.setAppBackgrounded(true) }
        assertEquals("", controller.status())
        assertFalse(controller.isRunning)
        assertTrue(backend.calls.isEmpty())
    }

    /** Starts run in order on the controller's thread, each with the settings of its time. */
    @Test
    fun asyncStartsRunInOrderAndReportOnTheMainThread() {
        val backend = FakeBackend()
        val controller = SyncController(settings, backend, "1.0")
        val done = CountDownLatch(3)
        val onMain = Collections.synchronizedList(mutableListOf<Boolean>())
        val report: (String?) -> Unit = {
            onMain += Looper.myLooper() == Looper.getMainLooper()
            done.countDown()
        }
        complete("s3://one")
        controller.startAsync(report)
        // Changed after the first was queued: still read in time by the later starts.
        controller.startAsync(report)
        complete("s3://two")
        controller.startAsync(report)
        controller.stopAsync()
        assertTrue(done.await(10, TimeUnit.SECONDS))
        // The stop has no callback: it runs after the starts, possibly after their reports.
        assertTrue(backend.stopped.await(10, TimeUnit.SECONDS))
        assertEquals(listOf(true, true, true), onMain)
        assertEquals("stop", backend.calls.last())
        assertEquals("start s3://two", backend.calls[backend.calls.size - 2])
        assertFalse(controller.isRunning)
    }

    @Test
    fun lifecycleReachesTheBackendOnTheMainThread() {
        val backend = FakeBackend()
        val controller = SyncController(settings, backend, "1.0")
        instrumentation.runOnMainSync {
            controller.attachToAppLifecycle()
            controller.attachToAppLifecycle() // once is enough
            controller.setAppBackgrounded(true)
            controller.setAppBackgrounded(false)
            controller.detachFromAppLifecycle()
        }
        assertEquals(listOf("backgrounded true", "backgrounded false"), backend.calls.filter { it.startsWith("backgrounded") })
    }
}
