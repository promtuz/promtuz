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

val repoRoot = rootProject.file("..")
val minSdkVersion = 26
val abis = listOf("arm64-v8a", "x86_64")

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

val secretsFile = rootProject.file("secrets.properties")
val secrets = Properties().apply {
    if (secretsFile.exists()) secretsFile.inputStream().use { load(it) }
}
val resolverSeedsLiteral = secrets.getProperty("RESOLVER_SEEDS", "")
    .replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", "\\n")

val sdkDir = Properties().apply {
    val lp = rootProject.file("local.properties")
    if (lp.exists()) lp.inputStream().use { load(it) }
}.getProperty("sdk.dir")
    ?: System.getenv("ANDROID_HOME")
    ?: System.getenv("ANDROID_SDK_ROOT")

val cargoBin = "${System.getProperty("user.home")}/.cargo/bin"
val cargo = file("$cargoBin/cargo").takeIf { it.exists() }?.absolutePath ?: "cargo"
val cargoNdk = file("$cargoBin/cargo-ndk").takeIf { it.exists() }?.absolutePath ?: "cargo-ndk"
val cargoAugmentedPath = "$cargoBin:${System.getenv("PATH") ?: ""}"

val uniffiOutDir = layout.buildDirectory.dir("generated/source/uniffi/kotlin").get().asFile.apply { mkdirs() }
val rustLicenseArtifacts = layout.buildDirectory.file("intermediates/licenses/rust-artifacts.jsonl")

android {
    namespace = "com.promtuz.chat"
    compileSdk = 37
    ndkVersion = "30.0.15729638"

    defaultConfig {
        applicationId = "com.promtuz.chat"
        minSdk = minSdkVersion
        targetSdk = 37
        versionCode = publishedVersionCode
        versionName = publishedVersionName

        buildConfigField("String", "RESOLVER_SEEDS", "\"$resolverSeedsLiteral\"")
    }
    splits {
        abi {
            isEnable = true
            reset()
            include(*abis.toTypedArray())
            isUniversalApk = false
        }
    }
    packaging {
        jniLibs {
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

    buildFeatures {
        compose = true
        buildConfig = true
    }
}

kotlin {
    compilerOptions {
        freeCompilerArgs.add("-opt-in=androidx.compose.material3.ExperimentalMaterial3Api")
        freeCompilerArgs.add("-opt-in=androidx.compose.material3.ExperimentalMaterial3ExpressiveApi")
    }
}

androidComponents {
    onVariants { variant ->
        variant.sources.java?.addStaticSourceDirectory(uniffiOutDir.absolutePath)

        val variantName = variant.name.replaceFirstChar { it.uppercase() }
        val licenseOutput = layout.buildDirectory.dir("generated/licenses/${variant.name}")
        val generateLicenses = tasks.register<Exec>("generate${variantName}LicenseAssets") {
            dependsOn("buildRustCore")
            val runtime = configurations.named("${variant.name}RuntimeClasspath")
            val generator = repoRoot.resolve("tools/licenses/generate.py")
            inputs.files(runtime)
            inputs.files(repoRoot.resolve("Cargo.lock"), repoRoot.resolve("Cargo.toml"),
                repoRoot.resolve("libcore/Cargo.toml"), repoRoot.resolve("common/Cargo.toml"), generator)
            inputs.file(rustLicenseArtifacts)
            inputs.dir(repoRoot.resolve("tools/licenses/notices"))
            outputs.dir(licenseOutput)
            environment("PATH", cargoAugmentedPath)
            workingDir = repoRoot
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
        tasks.configureEach {
            if (name == "merge${variantName}Assets" ||
                (name.contains("lint", ignoreCase = true) && name.contains(variantName))
            ) dependsOn(generateLicenses)
        }
    }
}

tasks.register<Exec>("buildRustCore") {
    val artifactOutput = rustLicenseArtifacts.get().asFile
    val pendingArtifacts = file("${artifactOutput.absolutePath}.pending")
    environment("CARGO", repoRoot.resolve("tools/licenses/cargo-artifacts.py").absolutePath)
    environment("PROMTUZ_REAL_CARGO", cargo)
    environment("PROMTUZ_RUST_ARTIFACTS", pendingArtifacts.absolutePath)
    val isRelease = gradle.startParameter.taskNames.any { task ->
        listOf("Release", "Benchmark").any { task.contains(it, ignoreCase = true) }
    }
    doFirst {
        logger.lifecycle("Compiling libcore for ${if (isRelease) "Release" else "Debug"} build")
        pendingArtifacts.parentFile.mkdirs()
        pendingArtifacts.writeText("")
    }
    doLast {
        pendingArtifacts.copyTo(artifactOutput, overwrite = true)
        pendingArtifacts.delete()
    }

    workingDir = repoRoot.resolve("libcore")

    val ndkDir = "${checkNotNull(sdkDir) {
        "Android SDK not found: set sdk.dir in local.properties, or ANDROID_HOME / ANDROID_SDK_ROOT"
    }}/ndk/${android.ndkVersion}"
    environment("ANDROID_NDK_HOME", ndkDir)
    environment("ANDROID_NDK_ROOT", ndkDir)
    environment("PATH", cargoAugmentedPath)

    val cargoNdkArgs = buildList {
        add("ndk")
        abis.forEach { addAll(listOf("-t", it)) }
        addAll(listOf(
            "-o", file("src/main/jniLibs").absolutePath,
            "--platform", minSdkVersion.toString(),
            "build",
        ))
        if (isRelease) add("--release")
    }
    commandLine(listOf(cargoNdk) + cargoNdkArgs)
}

tasks.register<Exec>("generateUniffiBindings") {
    dependsOn("buildRustCore")
    workingDir = repoRoot
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

tasks.named("preBuild") { dependsOn("generateUniffiBindings") }

dependencies {

    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.appcompat)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.ui)
    implementation(libs.androidx.ui.graphics)
    implementation(libs.androidx.ui.tooling.preview)
    implementation(libs.androidx.material3)
    implementation(libs.google.material)
    implementation(libs.haze.materials)

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

    implementation(libs.play.services.blockstore)
    implementation(libs.androidx.work.runtime.ktx)
    implementation(libs.androidx.lifecycle.process)

    implementation(project.dependencies.platform(libs.koin.bom))
    implementation(libs.koin.core)

    implementation(libs.koin.androidx.compose)

    implementation(libs.coil.compose)
    implementation(libs.coil.video)

    implementation(libs.androidx.camera.core)
    implementation(libs.androidx.camera.camera2)
    implementation(libs.androidx.camera.lifecycle)
    implementation(libs.androidx.camera.view)
    implementation(libs.androidx.camera.video)

    implementation(libs.androidx.media3.exoplayer)
    implementation(libs.androidx.media3.ui.compose)
    implementation(libs.androidx.media3.transformer)
    implementation(libs.androidx.media3.effect)
    implementation(libs.androidx.media3.inspector)

    implementation(libs.lottie.compose)

    implementation(libs.barcode.scanning)
    implementation(libs.zxing.core)

    implementation(libs.timber)

    implementation(libs.capturable)

    implementation(platform(libs.firebase.bom))

    implementation(libs.firebase.messaging)

    implementation(variantOf(libs.jna) { artifactType("aar") })
    implementation(libs.avif)
}
