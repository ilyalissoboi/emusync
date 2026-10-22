package io.github.ilyalissoboi.emusync.prefabtest

/** The consumer's JNI library, linked against libemusync.so from the AAR's Prefab package. */
object PrefabTest {
    init {
        System.loadLibrary("prefabtest")
    }

    @JvmStatic external fun abiVersion(): Int
    @JvmStatic external fun version(): String
    @JvmStatic external fun createClient(dataDir: String): String
}
