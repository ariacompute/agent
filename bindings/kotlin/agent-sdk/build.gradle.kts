plugins {
    id("com.android.library")
    kotlin("android")
}

android {
    namespace = "com.agent"
    compileSdk = 34

    defaultConfig {
        minSdk = 24
        consumerProguardFiles("consumer-rules.pro")
        ndk {
            // ABIs you intend to ship. Build libagent_sdk for each and drop the
            // .so into src/main/jniLibs/<abi>/libagent_sdk.so
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    sourceSets["main"].java.srcDirs("src/main/kotlin")
}

dependencies {
    implementation(kotlin("stdlib"))
}
