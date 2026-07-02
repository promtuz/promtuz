import com.android.build.gradle.internal.tasks.factory.dependsOn

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.jetbrains.kotlin.serialization)
}

android {
    namespace = "com.promtuz.chat"
    compileSdk = 36
    ndkVersion = "29.0.14206865"

    defaultConfig {
        applicationId = "com.promtuz.chat"
        minSdk = 31
        targetSdk = 36
        versionCode = 1
        versionName = "1.0"

        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"

        ndk {
            abiFilters.addAll(listOf("arm64-v8a", "armeabi-v7a", "x86_64", "x86"))
        }
    }
    packaging {
        jniLibs {
            useLegacyPackaging = true
        }
    }

    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
        }
    }

    sourceSets {
        getByName("main") {
            jniLibs.directories.add("src/main/jniLibs")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro"
            )
            signingConfig = signingConfigs.getByName("debug")
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
            freeCompilerArgs.add("-opt-in=androidx.camera.core.ExperimentalGetImage")
            freeCompilerArgs.add("-XXLanguage:+NestedTypeAliases")
        }
    }
    buildFeatures {
        compose = true
    }
}

tasks.register<Exec>("buildRustCore") {
    val isRelease =
        name.contains("Release", ignoreCase = true) || gradle.startParameter.taskNames.any {
            it.contains("Release", ignoreCase = true)
        }

    println("Compiling libcore for ${if (isRelease) "Release" else "Debug"} build")

    workingDir = file("../../libcore")

    // @formatter:off
    if (isRelease) commandLine(
        "cargo", "ndk",
        "-t", "armeabi-v7a",
        "-t", "arm64-v8a",
        "-t", "x86",
        "-t", "x86_64",
        "-o", "../android/app/src/main/jniLibs",
        "--platform", (android.defaultConfig.minSdk ?: 21).toString(),
        "build", "--release"
    ) else commandLine(
        "cargo", "ndk",
        "-t", "arm64-v8a",
        "-t", "x86_64",
        "-o", "../android/app/src/main/jniLibs",
        "--platform", (android.defaultConfig.minSdk ?: 21).toString(),
        "build" //, "--release"
    )
    // @formatter:on
}

tasks.preBuild.dependsOn("buildRustCore")

dependencies {

    implementation(libs.androidx.core.ktx)
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
    androidTestImplementation(libs.androidx.junit)
    androidTestImplementation(libs.androidx.espresso.core)
    androidTestImplementation(platform(libs.androidx.compose.bom))
    androidTestImplementation(libs.androidx.ui.test.junit4)
    debugImplementation(libs.androidx.ui.tooling)
    debugImplementation(libs.androidx.ui.test.manifest)

    implementation(libs.androidx.navigation3.ui)
    implementation(libs.androidx.navigation3.runtime)
    implementation(libs.androidx.lifecycle.viewmodel.navigation3)
    implementation(libs.androidx.material3.adaptive.navigation3)

    implementation(libs.androidx.core.splashscreen)

    implementation(libs.kotlinx.serialization.core)
    implementation(libs.kotlinx.serialization.cbor)

    implementation(libs.kotlinx.coroutines.core)
    implementation(libs.kotlinx.coroutines.android)

    implementation(project.dependencies.platform(libs.koin.bom))
    implementation(libs.koin.core)

    implementation(libs.koin.androidx.compose)
    implementation(libs.koin.androidx.compose.navigation)

    implementation(kotlin("reflect"))

    implementation(libs.androidx.camera.core)
    implementation(libs.androidx.camera.camera2)
    implementation(libs.androidx.camera.lifecycle)
    implementation(libs.androidx.camera.view)

    implementation(libs.barcode.scanning)
    implementation(libs.zxing.core)

    implementation(libs.timber)

    implementation(libs.capturable)
    testImplementation(kotlin("test"))
}