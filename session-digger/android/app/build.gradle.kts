plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.droidcon.digger"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.droidcon.digger"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"))
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    buildFeatures {
        compose = true
    }
    packaging {
        resources.excludes += "/META-INF/{AL2.0,LGPL2.1}"
    }

    sourceSets {
        getByName("main") {
            // cargo-ndk output lands here via scripts/build-android-rust.sh
            jniLibs.srcDir("src/main/jniLibs")
        }
    }
}

// Cross-compile the Rust core (libsession_digger.so) before every build.
// The script degrades to a no-op with a console note when the Rust
// toolchain is absent, so pure-Kotlin builds still work.
val buildRust by tasks.registering(Exec::class) {
    group = "build"
    description = "Cross-compiles the Rust core with cargo-ndk (arm64-v8a)."
    workingDir = rootProject.projectDir.parentFile
    commandLine("bash", "scripts/build-android-rust.sh")
    environment("NDK_DIR", if (project.hasProperty("ndkDir")) project.property("ndkDir") else "")
}

tasks.named("preBuild") {
    dependsOn(buildRust)
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2024.12.01")
    implementation(composeBom)
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    debugImplementation("androidx.compose.ui:ui-tooling")

    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
}
