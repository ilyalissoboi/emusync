/* JNI functions the instrumented test calls: the C ABI, linked through the AAR's Prefab package. */
#include <jni.h>
#include <stdio.h>

#include <emusync.h>

JNIEXPORT jint JNICALL
Java_io_github_ilyalissoboi_emusync_prefabtest_PrefabTest_abiVersion(JNIEnv* env, jclass cls) {
    (void)env;
    (void)cls;
    return (jint)es_abi_version();
}

JNIEXPORT jstring JNICALL
Java_io_github_ilyalissoboi_emusync_prefabtest_PrefabTest_version(JNIEnv* env, jclass cls) {
    (void)cls;
    return (*env)->NewStringUTF(env, es_version_string());
}

/* Creates a client in data_dir and destroys it: the database and worker thread run in an app's
   sandbox. Returns "OK" or the status name of the failure. */
JNIEXPORT jstring JNICALL
Java_io_github_ilyalissoboi_emusync_prefabtest_PrefabTest_createClient(JNIEnv* env, jclass cls, jstring data_dir) {
    (void)cls;
    const char* dir = (*env)->GetStringUTFChars(env, data_dir, NULL);
    es_config cfg = ES_CONFIG_INIT;
    cfg.data_dir = dir;
    cfg.device_name = "prefab-test";
    cfg.emulator_id = "emusync-prefab-test";
    es_client* c = NULL;
    es_status st = es_client_create(&cfg, &c);
    (*env)->ReleaseStringUTFChars(env, data_dir, dir);
    if (st != ES_OK)
        return (*env)->NewStringUTF(env, es_status_str(st));
    es_client_destroy(c, 2000);
    return (*env)->NewStringUTF(env, "OK");
}
