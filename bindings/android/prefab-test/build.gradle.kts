// A host app consuming the emusync AAR the way an emulator would: libemusync.so packaged from the
// AAR's jni/ and linked through its Prefab package. It depends on the built .aar file rather than
// on the :emusync project, because the Prefab package is added to the .aar itself.
plugins {
    alias(libs.plugins.android.application)
}

android {
    namespace = "io.github.ilyalissoboi.emusync.prefabtest"
    compileSdk = 37
    ndkVersion = "29.0.14206865"

    defaultConfig {
        applicationId = "io.github.ilyalissoboi.emusync.prefabtest"
        minSdk = 26
        targetSdk = 37
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        ndk {
            abiFilters += providers.gradleProperty("emusync.abis").orElse("arm64-v8a,x86_64").get().split(',')
        }
    }

    buildFeatures {
        prefab = true
    }

    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
            version = "3.31.6"
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    implementation(files(rootDir.resolve("emusync/build/outputs/aar/emusync-release.aar")).builtBy(":emusync:assembleRelease"))
    androidTestImplementation(libs.androidx.junit)
    androidTestImplementation(libs.androidx.test.runner)
}
