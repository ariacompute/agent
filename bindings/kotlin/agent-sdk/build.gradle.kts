import com.vanniktech.maven.publish.SonatypeHost
import org.gradle.plugins.signing.Sign

plugins {
    id("com.android.library")
    kotlin("android") version "2.0.21"
    id("com.vanniktech.maven.publish") version "0.30.0"
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
}

dependencies {
    implementation(kotlin("stdlib"))
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
        key != null && key.toString().trim().isNotEmpty()
    }
}
