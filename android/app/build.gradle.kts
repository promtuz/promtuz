import com.android.build.gradle.internal.tasks.factory.dependsOn
import java.io.FileInputStream
import java.util.Properties
import groovy.json.JsonOutput
import org.gradle.api.artifacts.component.ModuleComponentIdentifier
import org.gradle.api.artifacts.result.ResolvedArtifactResult
import org.gradle.maven.MavenModule
import org.gradle.maven.MavenPomArtifact
import javax.xml.parsers.DocumentBuilderFactory

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.jetbrains.kotlin.serialization)

    id("com.google.gms.google-services")
}

val localProperties = Properties().apply {
    val propertiesFile = rootProject.file("local.properties")
    if (propertiesFile.exists()) {
        load(FileInputStream(propertiesFile))
    }
}

val pythonExecutable = localProperties.getProperty("python.executable")
    ?.trim()?.takeIf { it.isNotEmpty() } ?: "python3"

// Release signing comes from gitignored keystore.properties or temporary
// release-script environment variables. Missing credentials leave release
// unsigned rather than silently using another signing identity.
val keystorePropsFile = rootProject.file("keystore.properties")
val keystoreProps = Properties().apply {
    if (keystorePropsFile.exists()) keystorePropsFile.inputStream().use { load(it) }
}
val hasReleaseSigning = keystorePropsFile.exists() &&
    listOf("storeFile", "storePassword", "keyAlias", "keyPassword")
        .all { keystoreProps.getProperty(it) != null }

val publishedVersionCode = providers.gradleProperty("promtuzVersionCode").get().toInt()
val publishedVersionName = providers.gradleProperty("promtuzVersionName").get()
val promptedSigning = mapOf(
    "storeFile" to System.getenv("PROMTUZ_ANDROID_KEYSTORE"),
    "storePassword" to System.getenv("PROMTUZ_ANDROID_STORE_PASSWORD"),
    "keyAlias" to System.getenv("PROMTUZ_ANDROID_KEY_ALIAS"),
    "keyPassword" to System.getenv("PROMTUZ_ANDROID_KEY_PASSWORD"),
)
val hasPromptedSigning = promptedSigning.values.all { !it.isNullOrBlank() }

// Debug signing credentials, chosen as ONE whole source — never merged field
// by field, which could pair one keystore's path with another's password and
// fail at signing time with nothing useful to say.
//
// The last branch is the interesting one: debug signs with the RELEASE key
// when one is configured. Same signer means a debug build installs straight
// over a release build and vice versa, with no uninstall and no data wipe —
// and UpdateRepository.verifyApk(), which compares signer digests, treats
// both channels as the same app. Falling through to null leaves AGP's default
// debug keystore in play, so a machine without the vault still builds.
val debugSigning: Map<String, String?>? = when {
    hasPromptedSigning -> promptedSigning
    localProperties.getProperty("debug.store.file") != null -> mapOf(
        "storeFile" to localProperties.getProperty("debug.store.file"),
        "storePassword" to localProperties.getProperty("debug.store.password"),
        "keyAlias" to localProperties.getProperty("debug.key.alias"),
        "keyPassword" to localProperties.getProperty("debug.key.password"),
    )
    hasReleaseSigning -> keystoreProps.let {
        mapOf(
            "storeFile" to it.getProperty("storeFile"),
            "storePassword" to it.getProperty("storePassword"),
            "keyAlias" to it.getProperty("keyAlias"),
            "keyPassword" to it.getProperty("keyPassword"),
        )
    }
    else -> null
}

// Resolver bootstrap seeds, injected from a gitignored secrets.properties so
// the OSS repo never commits infra endpoints. Format: <IPK_HEX>::<host[:port]>
// (port defaults to 40433 in libcore). Empty when absent -> no bundled resolver.
val secretsFile = rootProject.file("secrets.properties")
val secrets = Properties().apply {
    if (secretsFile.exists()) secretsFile.inputStream().use { load(it) }
}
val resolverSeedsLiteral = secrets.getProperty("RESOLVER_SEEDS", "")
    .replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", "\\n")

// SDK dir resolved the way AGP does (local.properties sdk.dir -> env), used to
// hand cargo-ndk an absolute NDK path (see buildRustCore).
val sdkDir = Properties().apply {
    val lp = rootProject.file("local.properties")
    if (lp.exists()) lp.inputStream().use { load(it) }
}.getProperty("sdk.dir")
    ?: System.getenv("ANDROID_HOME")
    ?: System.getenv("ANDROID_SDK_ROOT")

// Gradle's Exec resolves the command name via the JVM's PATH (NOT the task's
// environment map) — and a GUI-launched Android Studio has launchd's bare PATH
// without ~/.cargo/bin. So invoke cargo by absolute path; the PATH env is still
// set below for the rustc/ndk toolchain cargo-ndk re-spawns. (Homebrew: repoint cargoBin.)
val cargoBin = "${System.getProperty("user.home")}/.cargo/bin"
val cargo = file("$cargoBin/cargo").takeIf { it.exists() }?.absolutePath ?: "cargo"
val cargoNdk = file("$cargoBin/cargo-ndk").takeIf { it.exists() }?.absolutePath ?: "cargo-ndk"
val cargoAugmentedPath = "$cargoBin:${System.getenv("PATH") ?: ""}"

// Generated uniffi Kotlin bindings land here (see generateUniffiBindings).
// mkdirs at config time so the Variant API can register it as a source dir.
val uniffiOutDir = layout.buildDirectory.dir("generated/source/uniffi/kotlin").get().asFile.apply { mkdirs() }
val rustLicenseArtifacts = layout.buildDirectory.file("intermediates/licenses/rust-artifacts.jsonl")

android {
    namespace = "com.promtuz.chat"
    compileSdk = 37
    ndkVersion = "29.0.14206865"

    defaultConfig {
        applicationId = "com.promtuz.chat"
        minSdk = 26
        targetSdk = 37
        versionCode = publishedVersionCode
        versionName = publishedVersionName


        buildConfigField("String", "RESOLVER_SEEDS", "\"$resolverSeedsLiteral\"")

    }
    splits {
        abi {
            isEnable = true
            reset()
            include("arm64-v8a", "x86_64")
            isUniversalApk = false
        }
    }
    packaging {
        jniLibs {
            // false => .so ship uncompressed + 16KB-page-aligned (libcore.so + JNA's jnidispatch.so).
            useLegacyPackaging = false
        }
    }

    sourceSets {
        getByName("main") {
            jniLibs.directories.add("src/main/jniLibs")
        }
    }

    signingConfigs {
        getByName("debug") {
            debugSigning?.let {
                storeFile = rootProject.file(it["storeFile"]!!)
                storePassword = it["storePassword"]
                keyAlias = it["keyAlias"]
                keyPassword = it["keyPassword"]
            }
        }
        // rootProject.file, not file: keystore.properties is read from the root
        // project, so a relative storeFile in it must resolve there too — plain
        // file() would resolve against app/ and miss. Absolute paths (what the
        // release script exports) pass through either way.
        if (hasPromptedSigning || hasReleaseSigning) create("release") {
            storeFile = rootProject.file(promptedSigning["storeFile"] ?: keystoreProps.getProperty("storeFile"))
            storePassword = promptedSigning["storePassword"] ?: keystoreProps.getProperty("storePassword")
            keyAlias = promptedSigning["keyAlias"] ?: keystoreProps.getProperty("keyAlias")
            keyPassword = promptedSigning["keyPassword"] ?: keystoreProps.getProperty("keyPassword")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro"
            )
            signingConfig = if (hasPromptedSigning || hasReleaseSigning) signingConfigs.getByName("release") else null
        }
        // Perf measurement: AOT-compiled, non-debuggable (no Compose debug checks,
        // no JIT cold start), debug-signed so it installs anywhere. Minify stays
        // off to keep uniffi/JNA out of R8's reach — the wins we're measuring are
        // debuggable=false + AOT, not shrinking. `gradlew installBenchmark`.
        create("benchmark") {
            initWith(getByName("release"))
            isMinifyEnabled = false
            isShrinkResources = false
            isDebuggable = false
            signingConfig = signingConfigs.getByName("debug")
            matchingFallbacks += listOf("release")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }

    kotlin {
        compilerOptions {
            freeCompilerArgs.add("-opt-in=androidx.compose.material3.ExperimentalMaterial3Api")
            freeCompilerArgs.add("-opt-in=androidx.compose.material3.ExperimentalMaterial3ExpressiveApi")
            freeCompilerArgs.add("-XXLanguage:+NestedTypeAliases")
        }
    }
    buildFeatures {
        compose = true
        buildConfig = true
    }
    ndkVersion = "29.0.14206865"
}

// Register the generated uniffi bindings as a Kotlin source dir per variant
// (AGP 9 wants source dirs via the Variant API, not the sourceSets DSL).
// generateUniffiBindings populates it before compile (via preBuild ordering).
androidComponents {
    onVariants { variant ->
        variant.sources.java?.addStaticSourceDirectory("build/generated/source/uniffi/kotlin")

        val variantName = variant.name.replaceFirstChar { it.uppercase() }
        val licenseOutput = layout.buildDirectory.dir("generated/licenses/${variant.name}")
        val generateLicenses = tasks.register<Exec>("generate${variantName}LicenseAssets") {
            dependsOn("buildRustCore")
            val runtime = configurations.named("${variant.name}RuntimeClasspath")
            val generator = rootProject.file("../tools/licenses/generate.py")
            inputs.files(runtime)
            inputs.files(rootProject.file("../Cargo.lock"), rootProject.file("../Cargo.toml"),
                rootProject.file("../libcore/Cargo.toml"), rootProject.file("../common/Cargo.toml"), generator)
            inputs.file(rustLicenseArtifacts)
            inputs.dir(rootProject.file("../tools/licenses/notices"))
            outputs.dir(licenseOutput)
            environment("PATH", cargoAugmentedPath)
            workingDir = rootProject.file("..")
            val inventory = layout.buildDirectory.file("intermediates/licenses/${variant.name}.json")
            doFirst {
                val artifacts = runtime.get().resolvedConfiguration.resolvedArtifacts
                    .distinctBy { it.moduleVersion.id.toString() to it.file }
                val ids = artifacts.mapNotNull { it.id.componentIdentifier as? ModuleComponentIdentifier }.distinct()
                val pomResult = dependencies.createArtifactResolutionQuery()
                    .forComponents(ids)
                    .withArtifacts(MavenModule::class.java, MavenPomArtifact::class.java)
                    .execute()
                val poms = pomResult.resolvedComponents.associate { component ->
                        component.id.displayName to component.getArtifacts(MavenPomArtifact::class.java)
                            .filterIsInstance<ResolvedArtifactResult>().firstOrNull()?.file?.absolutePath
                    }.toMutableMap()
                val xml = DocumentBuilderFactory.newInstance().apply {
                    setFeature("http://apache.org/xml/features/disallow-doctype-decl", true)
                }.newDocumentBuilder()
                val inspected = mutableSetOf<String>()
                while (true) {
                    val parents = poms.values.filterNotNull().filter { inspected.add(it) }.mapNotNull { path ->
                        val parent = xml.parse(file(path)).getElementsByTagName("parent").item(0)
                            as? org.w3c.dom.Element ?: return@mapNotNull null
                        listOf("groupId", "artifactId", "version").joinToString(":") {
                            parent.getElementsByTagName(it).item(0).textContent.trim()
                        }
                    }.filter { it !in poms }.distinct()
                    if (parents.isEmpty()) break
                    val query = dependencies.createArtifactResolutionQuery()
                    parents.forEach { id ->
                        val (group, name, version) = id.split(":")
                        query.forModule(group, name, version)
                        poms[id] = null
                    }
                    query.withArtifacts(MavenModule::class.java, MavenPomArtifact::class.java)
                        .execute().resolvedComponents.forEach { component ->
                            poms[component.id.displayName] = component.getArtifacts(MavenPomArtifact::class.java)
                                .filterIsInstance<ResolvedArtifactResult>().firstOrNull()?.file?.absolutePath
                        }
                }
                val entries = artifacts.map { artifact ->
                    mapOf(
                        "id" to artifact.moduleVersion.id.toString(),
                        "artifact" to artifact.file.absolutePath,
                        "pom" to poms[artifact.id.componentIdentifier.displayName],
                    )
                }
                inventory.get().asFile.apply {
                    parentFile.mkdirs()
                    writeText(JsonOutput.toJson(mapOf("libraries" to entries, "poms" to poms)))
                }
            }
            commandLine(pythonExecutable, generator.absolutePath,
                "--android", inventory.get().asFile.absolutePath,
                "--output", licenseOutput.get().asFile.absolutePath,
                "--rust-artifacts", rustLicenseArtifacts.get().asFile.absolutePath)
        }
        variant.sources.assets?.addStaticSourceDirectory(licenseOutput.get().asFile.absolutePath)
        tasks.matching {
            it.name == "merge${variantName}Assets" ||
                (it.name.contains("lint", ignoreCase = true) && it.name.contains(variantName))
        }.configureEach { dependsOn(generateLicenses) }
    }
}

tasks.register<Exec>("buildRustCore") {
    val artifactOutput = rustLicenseArtifacts.get().asFile
    val pendingArtifacts = file("${artifactOutput.absolutePath}.pending")
    environment("CARGO", rootProject.file("../tools/licenses/cargo-artifacts.py").absolutePath)
    environment("PROMTUZ_REAL_CARGO", cargo)
    environment("PROMTUZ_RUST_ARTIFACTS", pendingArtifacts.absolutePath)
    doFirst {
        pendingArtifacts.parentFile.mkdirs()
        pendingArtifacts.writeText("")
    }
    doLast {
        pendingArtifacts.copyTo(artifactOutput, overwrite = true)
        pendingArtifacts.delete()
    }
    val isRelease =
        name.contains("Release", ignoreCase = true) || gradle.startParameter.taskNames.any {
            it.contains("Release", ignoreCase = true)
        }

    println("Compiling libcore for ${if (isRelease) "Release" else "Debug"} build")

    workingDir = file("../../libcore")

    // Hand cargo-ndk an absolute NDK path derived from AGP's own ndkVersion,
    // so the build never depends on a tilde'd / unset ambient ANDROID_NDK_ROOT
    // (which fails outside an interactive shell — Android Studio, CI, daemons).
    val ndkDir = "$sdkDir/ndk/${android.ndkVersion}"
    environment("ANDROID_NDK_HOME", ndkDir)
    environment("ANDROID_NDK_ROOT", ndkDir)
    environment("PATH", cargoAugmentedPath)

    // @formatter:off
    if (isRelease) commandLine(
        cargoNdk, "ndk",
        "-t", "arm64-v8a",
        "-t", "x86_64",
        "-o", "../android/app/src/main/jniLibs",
        "--platform", (android.defaultConfig.minSdk ?: 21).toString(),
        "build", "--release"
    ) else commandLine(
        cargoNdk, "ndk",
        "-t", "arm64-v8a",
        "-t", "x86_64",
        "-o", "../android/app/src/main/jniLibs",
        "--platform", (android.defaultConfig.minSdk ?: 21).toString(),
        "build"
    )
    // @formatter:on
}

// Generate the uniffi Kotlin bindings from the built .so (library mode).
// Bindings are identical across ABIs, so point --library at one (arm64-v8a).
tasks.register<Exec>("generateUniffiBindings") {
    dependsOn("buildRustCore")
    workingDir = file("../..") // cargo workspace root
    environment("PATH", cargoAugmentedPath)
    val outDir = uniffiOutDir
    doFirst { outDir.mkdirs() }
    commandLine(
        cargo, "run", "--quiet", "-p", "uniffi-bindgen", "--",
        "generate",
        "--library", "android/app/src/main/jniLibs/arm64-v8a/libcore.so",
        "--language", "kotlin",
        "--out-dir", outDir.absolutePath,
    )
}

tasks.preBuild.dependsOn("buildRustCore")
tasks.preBuild.dependsOn("generateUniffiBindings")

dependencies {

    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.appcompat)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.constraintlayout.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.ui)
    implementation(libs.androidx.ui.graphics)
    implementation(libs.androidx.ui.tooling.preview)
    implementation(libs.androidx.material3)
    implementation(libs.google.material)
    implementation(libs.haze.materials)
//    implementation(libs.room.runtime)
//    implementation(libs.room.ktx)

    testImplementation(libs.junit)
    testImplementation(libs.kotlinx.coroutines.test)
    debugImplementation(libs.androidx.ui.tooling)

    implementation(libs.androidx.navigation3.ui)
    implementation(libs.androidx.navigation3.runtime)
    implementation(libs.androidx.lifecycle.viewmodel.navigation3)
    implementation(libs.androidx.material3.adaptive.navigation3)

    implementation(libs.androidx.core.splashscreen)

    implementation(libs.kotlinx.serialization.core)
    implementation(libs.kotlinx.serialization.json)

    implementation(libs.kotlinx.coroutines.core)
    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.kotlinx.coroutines.play.services)

    // Identity recovery: Block Store escrow + daily backup-blob worker.
    implementation(libs.play.services.blockstore)
    implementation(libs.androidx.work.runtime.ktx)
    implementation(libs.androidx.lifecycle.process)

    implementation(project.dependencies.platform(libs.koin.bom))
    implementation(libs.koin.core)

    implementation(libs.koin.androidx.compose)
    implementation(libs.koin.androidx.compose.navigation)

    implementation(kotlin("reflect"))

    implementation(libs.coil.compose)
    implementation(libs.coil.video)

    implementation(libs.androidx.camera.core)
    implementation(libs.androidx.camera.camera2)
    implementation(libs.androidx.camera.lifecycle)
    implementation(libs.androidx.camera.view)
    implementation(libs.androidx.camera.video)

    // Video playback in the media viewer.
    implementation(libs.androidx.media3.exoplayer)
    implementation(libs.androidx.media3.ui.compose)

    implementation(libs.lottie.compose)

    implementation(libs.barcode.scanning)
    implementation(libs.zxing.core)

    implementation(libs.timber)

    implementation(libs.capturable)

    implementation(platform(libs.firebase.bom))

    implementation(libs.firebase.messaging)

    // uniffi Kotlin bindings run on JNA. MUST be @aar (bundles the per-ABI
    // jnidispatch.so; a plain jar throws UnsatisfiedLinkError at the first FFI
    // call). A version-catalog alias can't carry the @aar classifier, so pin it
    // here with the catalog version. >=5.17 is 16KB-page-safe.
    implementation("net.java.dev.jna:jna:${libs.versions.jna.get()}@aar")

    // Bundled AVIF decoder (libavif + dav1d). The platform ImageDecoder is
    // device-specific — absent on API 26–30 and some 31+ builds reject our
    // encoder's output — so decode goes through this first.
    implementation("org.aomedia.avif.android:avif:1.3.0.841110fd")

    testImplementation(kotlin("test"))
}
