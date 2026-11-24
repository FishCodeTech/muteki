plugins { id("com.android.application") }

android {
    namespace = "com.tryanks.tcode"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.tryanks.tcode"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = System.getenv("TCODE_BUILD_VERSION") ?: "0.1.0"
        testInstrumentationRunner = "android.test.InstrumentationTestRunner"
        ndk { abiFilters += "arm64-v8a" }
    }

    // Android only installs an update signed with the same key as the installed
    // build, so release APKs need one fixed key across machines and CI runs. Set all
    // four TCODE_ANDROID_* variables to sign with it; with none set, release builds
    // fall back to Gradle's per-machine debug keystore (see docs/remote.md).
    signingConfigs {
        val releaseKeyVars = listOf(
            "TCODE_ANDROID_KEYSTORE",
            "TCODE_ANDROID_KEYSTORE_PASSWORD",
            "TCODE_ANDROID_KEY_ALIAS",
            "TCODE_ANDROID_KEY_PASSWORD",
        )
        val releaseKey = releaseKeyVars.associateWith { System.getenv(it)?.takeIf(String::isNotEmpty) }
        val missing = releaseKey.filterValues { it == null }.keys
        if (missing.isEmpty()) {
            val keystore = file(releaseKey.getValue("TCODE_ANDROID_KEYSTORE")!!)
            if (!keystore.isFile) throw GradleException("TCODE_ANDROID_KEYSTORE does not exist: $keystore")
            create("release") {
                storeFile = keystore
                storePassword = releaseKey.getValue("TCODE_ANDROID_KEYSTORE_PASSWORD")
                keyAlias = releaseKey.getValue("TCODE_ANDROID_KEY_ALIAS")
                keyPassword = releaseKey.getValue("TCODE_ANDROID_KEY_PASSWORD")
            }
        } else if (missing.size < releaseKeyVars.size) {
            throw GradleException("Android release signing partially configured; missing: ${missing.joinToString(" ")}")
        }
    }

    buildTypes {
        debug { isJniDebuggable = true }
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            signingConfig = signingConfigs.findByName("release") ?: signingConfigs.getByName("debug")
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    packaging { jniLibs { useLegacyPackaging = false } }
}

dependencies {
    // The device provides the platform test runner; only its compile stubs are needed here.
    androidTestCompileOnly(files("${android.sdkDirectory}/platforms/android-${android.compileSdk}/optional/android.test.base.jar"))
    implementation("androidx.webkit:webkit:1.12.1")
    implementation("androidx.core:core:1.15.0")
    implementation("androidx.core:core-splashscreen:1.0.1")
    implementation("androidx.fragment:fragment:1.8.5")
    implementation("androidx.camera:camera-core:1.4.2")
    implementation("androidx.camera:camera-camera2:1.4.2")
    implementation("androidx.camera:camera-lifecycle:1.4.2")
    implementation("androidx.camera:camera-view:1.4.2")
    implementation("com.google.mlkit:barcode-scanning:17.3.0")
}
