package io.github.ilyalissoboi.emusync.prefabtest

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/** A host app links and loads libemusync.so from the AAR, and the library runs in its sandbox. */
@RunWith(AndroidJUnit4::class)
class PrefabTestTest {
    @Test
    fun linksAndRunsTheLibrary() {
        assertEquals(1, PrefabTest.abiVersion())
        assertTrue(PrefabTest.version(), PrefabTest.version().contains("abi 1"))
        val dir = File(InstrumentationRegistry.getInstrumentation().targetContext.noBackupFilesDir, "emusync-prefab-test")
        dir.deleteRecursively()
        assertEquals("OK", PrefabTest.createClient(dir.path))
        assertTrue(File(dir, "state.db").exists() || dir.listFiles().orEmpty().isNotEmpty())
    }
}
