import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

// The release key: android/keystore.properties on the PC (never committed), or GALDR_KEYSTORE*
// environment variables in CI. Without either, release builds use the debug key.
val releaseKey: Map<String, String>? = run {
    val file = rootProject.file("keystore.properties")
    val props = Properties()
    if (file.exists()) file.inputStream().use(props::load)
    val get = { key: String, env: String -> props.getProperty(key) ?: System.getenv(env) }
    val values = mapOf(
        "storeFile" to get("storeFile", "GALDR_KEYSTORE"),
        "storePassword" to get("storePassword", "GALDR_KEYSTORE_PASSWORD"),
        "keyAlias" to get("keyAlias", "GALDR_KEY_ALIAS"),
        "keyPassword" to get("keyPassword", "GALDR_KEY_PASSWORD"),
    )
    if (values.values.all { !it.isNullOrEmpty() }) values.mapValues { it.value!! } else null
}

android {
    namespace = "io.github.huntez.galdr"
    compileSdk = 37

    defaultConfig {
        applicationId = "io.github.huntez.galdr"
        minSdk = 30 // microphone foreground service type
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        ndk {
            // 64- and 32-bit ARM phones; x86_64 for the emulator. ~0.3 MB of libopus each.
            abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64")
        }
    }

    // Microphone (AAudio) and Opus (libopus, a git submodule) in native code: src/main/cpp.
    ndkVersion = "30.0.16248370"
    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
            version = "4.1.2"
        }
    }

    signingConfigs {
        if (releaseKey != null) {
            create("release") {
                storeFile = rootProject.file(releaseKey.getValue("storeFile"))
                storePassword = releaseKey.getValue("storePassword")
                keyAlias = releaseKey.getValue("keyAlias")
                keyPassword = releaseKey.getValue("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"))
            signingConfig = signingConfigs.findByName("release") ?: signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
    }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2026.09.00"))
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui-tooling-preview")
    debugImplementation("androidx.compose.ui:ui-tooling")
}
