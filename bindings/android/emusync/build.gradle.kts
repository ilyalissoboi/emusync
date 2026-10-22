// The emusync AAR (DESIGN §26): the C library for the host's native code, as jniLibs for packaging
// and as a Prefab package for its CMake build, plus the Kotlin side of an Android integration.
import com.android.build.api.artifact.SingleArtifact
import java.util.zip.ZipEntry
import java.util.zip.ZipFile
import java.util.zip.ZipOutputStream
import javax.inject.Inject
import org.gradle.process.ExecOperations

plugins {
    alias(libs.plugins.android.library)
}

/** The emusync checkout this module lives in. */
val emusyncCheckout: File = rootDir.resolve("../..").canonicalFile

/** Android ABI → Rust target. armeabi-v7a and x86 build too but are untested (DESIGN §21). */
val emusyncAbis = providers.gradleProperty("emusync.abis").orElse("arm64-v8a,x86_64")
    .map { it.split(',').map(String::trim).filter(String::isNotEmpty) }
val rustTargetForAbi = mapOf(
    "arm64-v8a" to "aarch64-linux-android",
    "x86_64" to "x86_64-linux-android",
    "armeabi-v7a" to "armv7-linux-androideabi",
    "x86" to "i686-linux-android",
)
/**
 * Android 7, Dolphin's minimum. The library links only libc functions Android 7 has; its one newer
 * one, getrandom (Android 9), is a weak reference the getrandom crate checks before calling.
 */
val minSdkLevel = 24
val ndkVersionString = "29.0.14206865"

/** The library version: the workspace's, from Cargo.toml. */
val emusyncVersion: String = Regex("""(?m)^version\s*=\s*"([^"]+)"""")
    .find(emusyncCheckout.resolve("Cargo.toml").readText())!!.groupValues[1]

android {
    namespace = "io.github.ilyalissoboi.emusync"
    compileSdk = 37
    ndkVersion = ndkVersionString

    defaultConfig {
        minSdk = minSdkLevel
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

dependencies {
    implementation(libs.androidx.lifecycle.process)
    androidTestImplementation(libs.androidx.junit)
    androidTestImplementation(libs.androidx.test.runner)
}

/**
 * Builds libemusync.so for each ABI with scripts/build-android.sh, the build CI checks
 * (scripts/check-android-so.sh), into `<outDir>/<abi>/libemusync.so`. Cargo decides what to
 * rebuild, so the task always runs; a no-op build takes about a second per ABI. The output folder
 * is emptied first: AGP packages all of it, and an ABI left from an earlier build with other
 * `emusync.abis` would be packaged too, possibly out of date.
 */
abstract class BuildEmusyncLibrary @Inject constructor(private val exec: ExecOperations) : DefaultTask() {
    @get:Input abstract val rustTargets: ListProperty<String>
    @get:Input abstract val apiLevel: Property<Int>
    @get:Internal abstract val emusyncRoot: DirectoryProperty
    @get:Internal abstract val ndkDirectory: DirectoryProperty
    @get:Input abstract val bash: Property<String>
    @get:OutputDirectory abstract val outDir: DirectoryProperty

    init {
        outputs.upToDateWhen { false }
    }

    @TaskAction
    fun build() {
        val root = emusyncRoot.get().asFile
        val out = outDir.get().asFile
        out.deleteRecursively()
        out.mkdirs()
        val args = mutableListOf(root.resolve("scripts/build-android.sh").path, "--api", apiLevel.get().toString())
        rustTargets.get().forEach { args += listOf("--target", it) }
        args += listOf("--out", out.path)
        exec.exec {
            executable = bash.get()
            args(args)
            workingDir = root
            environment("ANDROID_NDK_HOME", ndkDirectory.get().asFile.path)
        }
    }
}

/**
 * Adds a Prefab package (https://google.github.io/prefab/) to the AAR: `emusync.h` and each ABI's
 * libemusync.so, so a host's CMake build finds them with `find_package(emusync REQUIRED CONFIG)`
 * and links `emusync::emusync`. AGP's own Prefab publishing only publishes libraries its CMake
 * build links itself, and libemusync.so is linked by cargo.
 */
abstract class AddPrefabPackage : DefaultTask() {
    @get:InputFile abstract val inputAar: RegularFileProperty
    @get:InputDirectory abstract val libraries: DirectoryProperty
    @get:InputFile abstract val header: RegularFileProperty
    @get:Input abstract val abis: ListProperty<String>
    @get:Input abstract val version: Property<String>
    @get:Input abstract val apiLevel: Property<Int>
    @get:Input abstract val ndkMajor: Property<Int>
    @get:OutputFile abstract val outputAar: RegularFileProperty

    @TaskAction
    fun add() {
        val module = "prefab/modules/emusync"
        ZipFile(inputAar.get().asFile).use { input ->
            ZipOutputStream(outputAar.get().asFile.outputStream().buffered()).use { out ->
                for (entry in input.entries()) {
                    if (entry.name.startsWith("prefab/")) continue
                    out.putNextEntry(ZipEntry(entry.name))
                    input.getInputStream(entry).use { it.copyTo(out) }
                    out.closeEntry()
                }
                fun put(name: String, bytes: ByteArray) {
                    out.putNextEntry(ZipEntry(name))
                    out.write(bytes)
                    out.closeEntry()
                }
                put("prefab/prefab.json", """{"schema_version":2,"name":"emusync","version":"${version.get()}","dependencies":[]}""".toByteArray())
                put("$module/module.json", """{"export_libraries":[],"android":{}}""".toByteArray())
                put("$module/include/emusync.h", header.get().asFile.readBytes())
                for (abi in abis.get()) {
                    val lib = libraries.get().asFile.resolve("$abi/libemusync.so")
                    check(lib.isFile) { "missing $lib" }
                    val dir = "$module/libs/android.$abi"
                    put("$dir/abi.json", """{"abi":"$abi","api":${apiLevel.get()},"ndk":${ndkMajor.get()},"stl":"none","static":false}""".toByteArray())
                    put("$dir/libemusync.so", lib.readBytes())
                }
            }
        }
    }
}

val buildEmusyncLibrary = tasks.register<BuildEmusyncLibrary>("buildEmusyncLibrary") {
    rustTargets.set(emusyncAbis.map { abis -> abis.map { rustTargetForAbi.getValue(it) } })
    apiLevel.set(minSdkLevel)
    emusyncRoot.set(emusyncCheckout)
    ndkDirectory.set(androidComponents.sdkComponents.ndkDirectory)
    // On Windows, Git's bash: a plain "bash" there can be WSL's.
    val gitBash = File("C:/Program Files/Git/bin/bash.exe")
    bash.set(providers.gradleProperty("emusync.bash").orElse(if (gitBash.isFile) gitBash.path else "bash"))
    outDir.set(layout.buildDirectory.dir("generated/emusync/jniLibs"))
}

androidComponents {
    onVariants { variant ->
        variant.sources.jniLibs?.addGeneratedSourceDirectory(buildEmusyncLibrary, BuildEmusyncLibrary::outDir)
        val name = variant.name.replaceFirstChar(Char::uppercase)
        val prefab = tasks.register<AddPrefabPackage>("addPrefabPackage$name") {
            libraries.set(buildEmusyncLibrary.flatMap { it.outDir })
            header.set(emusyncCheckout.resolve("include/emusync.h"))
            abis.set(emusyncAbis)
            version.set(emusyncVersion)
            apiLevel.set(minSdkLevel)
            ndkMajor.set(ndkVersionString.substringBefore('.').toInt())
        }
        variant.artifacts.use(prefab)
            .wiredWithFiles(AddPrefabPackage::inputAar, AddPrefabPackage::outputAar)
            .toTransform(SingleArtifact.AAR)
    }
}
