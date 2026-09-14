import com.vanniktech.maven.publish.SonatypeHost
import org.gradle.plugins.signing.Sign

plugins {
    id("com.android.library") version "8.5.0"
    kotlin("android") version "2.0.21"
    id("com.vanniktech.maven.publish") version "0.30.0"
    id("signing")
}

group = "com.ariacompute"
version = System.getenv("ARIA_VERSION") ?: "0.1.0"

android {
    namespace = "com.ariacompute.agent"
    compileSdk = 34

    defaultConfig {
        minSdk = 24
        consumerProguardFiles("consumer-rules.pro")
        ndk {
            // ABIs you intend to ship. Build libaria-agent_ffi for each and drop the
            // .so into src/main/jniLibs/<abi>/libaria-agent_ffi.so
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    sourceSets["main"].java.srcDirs("src/main/kotlin")

    lint {
        // The generated UniFFI Kotlin bindings reference `java.lang.ref.Cleaner`
        // (Android API 33) only inside a runtime `Class.forName` guard that
        // falls back to the JNA cleaner on older runtimes. Lint's `NewApi` check
        // cannot see the guard, so it reports a false positive. Disable it for
        // this generated-only source set (regeneration via `just ffi` keeps it).
        disable += "NewApi"
    }
}

dependencies {
    implementation(kotlin("stdlib"))
    // UniFFI Kotlin bindings are JNA-based: the generated aria_agent_ffi.kt
    // references com.sun.jna.* (Structure/Pointer/Library/Native/Cleaner...).
    // Use the AAR so libjnidispatch.so for the configured ABIs is bundled.
    implementation("net.java.dev.jna:jna:5.14.0@aar")
}

mavenPublishing {
    publishToMavenCentral(SonatypeHost.CENTRAL_PORTAL, true)
    signAllPublications()

    coordinates("com.ariacompute", "agent", version.toString())

    pom {
        name = "Aria Agent"
        description = "Kotlin/Android binding for the Aria agent platform (libaria-agent_ffi FFI)."
        url = "https://github.com/ariacompute/agent"
        licenses {
            license {
                name = "MIT License"
                url = "https://opensource.org/licenses/MIT"
            }
        }
        developers {
            developer {
                id = "ariacompute"
                name = "AriaCompute"
                url = "https://github.com/ariacompute"
            }
        }
        scm {
            url = "https://github.com/ariacompute/agent"
            connection = "scm:git:git://github.com/ariacompute/agent.git"
            developerConnection = "scm:git:ssh://git@github.com/ariacompute/agent.git"
        }
    }
}

fun preparePgpKey(raw: String): String {
    var key = raw.replace("\\n", "\n").replace("\r\n", "\n").trim()
    if (key.contains("-----BEGIN PGP")) {
        return key
    }
    val body = key.replace("\n", "").trim()
    return "-----BEGIN PGP PRIVATE KEY BLOCK-----\n\n${body}\n-----END PGP PRIVATE KEY BLOCK-----\n"
}

val pgpKeyRaw: String? = System.getenv("GPG_PRIVATE_KEY") ?: findProperty("signingInMemoryKey")?.toString()
val pgpPassword: String =
    System.getenv("GPG_PASSPHRASE") ?: findProperty("signingInMemoryKeyPassword")?.toString() ?: ""

if (!pgpKeyRaw.isNullOrBlank()) {
    signing {
        useInMemoryPgpKeys(preparePgpKey(pgpKeyRaw), pgpPassword)
    }
}

tasks.withType<Sign>().configureEach {
    onlyIf {
        val key = System.getenv("GPG_PRIVATE_KEY") ?: findProperty("signingInMemoryKey")?.toString()
        key != null && key.trim().isNotEmpty()
    }
}
