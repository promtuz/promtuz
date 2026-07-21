package com.promtuz.chat.update

import android.content.Context
import android.content.Intent
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.Settings
import androidx.core.content.FileProvider
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import timber.log.Timber
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.Locale
import java.util.concurrent.atomic.AtomicBoolean

sealed interface UpdateState {
    data object None : UpdateState
    data object Checking : UpdateState
    data class Available(val manifest: UpdateManifest) : UpdateState
    data class Downloading(val manifest: UpdateManifest, val progress: Float) : UpdateState
    data class Ready(val manifest: UpdateManifest, val apk: File) : UpdateState
    data class PermissionNeeded(val manifest: UpdateManifest, val apk: File) : UpdateState
    data class Error(val message: String) : UpdateState
}

@Serializable
data class UpdateManifest(
    val versionCode: Int,
    val versionName: String,
    val apk: String,
    val sha256: String,
    val size: Long,
    val publishedAt: String,
)

class UpdateRepository(private val context: Context) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val checking = AtomicBoolean(false)
    private var downloadJob: Job? = null
    private val json = Json { ignoreUnknownKeys = false; isLenient = false }
    private val _state = MutableStateFlow<UpdateState>(UpdateState.None)
    val state: StateFlow<UpdateState> = _state.asStateFlow()


    companion object {
        private const val TAG = "AppUpdater"
        private val log = { Timber.tag(TAG) }
    }

    private val nativeChannel = if (context.applicationInfo.flags and android.content.pm.ApplicationInfo.FLAG_DEBUGGABLE != 0) {
        "debug"
    } else {
        "release"
    }
    val channel: String get() = ChatPrefs.updateChannel ?: nativeChannel

    /** Cross-channel switch: drop any in-flight/staged update from the old channel, then re-check. */
    fun switchChannel(newChannel: String) {
        if (newChannel == channel) return
        ChatPrefs.updateChannel = newChannel
        scope.launch {
            downloadJob?.cancelAndJoin()
            _state.value = UpdateState.None
            check()
        }
    }

    // Same-versionCode installs are allowed when crossing channels (the binaries
    // differ); the OS rejects downgrades either way.
    private fun minInstallableCode(): Long =
        installedVersionCode() + if (channel == nativeChannel) 1 else 0

    fun check() {
        // A foreground auto-check must not stomp an update the user is already
        // downloading or about to install — the verified APK is on disk; don't send them back to "Download".
        when (_state.value) {
            is UpdateState.Downloading, is UpdateState.Ready, is UpdateState.PermissionNeeded -> return
            else -> {}
        }
        if (!checking.compareAndSet(false, true)) return
        scope.launch {
            try {
                log().v("Checking for Updates...")
                _state.value = UpdateState.Checking
                val abi = supportedAbi()
                val manifestUrl = manifestUrl(abi)
                val rawManifest = getBytes(manifestUrl)

                val signature = getBytes("$manifestUrl.sig")
                require(CoreBridge.verifyUpdateManifest(rawManifest, signature)) { "Update signature could not be verified." }
                val manifest = json.decodeFromString<UpdateManifest>(rawManifest.decodeToString())
                validateManifest(manifest, abi)
                if (manifest.versionCode.toLong() >= minInstallableCode()) {
                    _state.value = UpdateState.Available(manifest)
                } else {
                    _state.value = UpdateState.None
                }
            } catch (error: Exception) {
                _state.value = UpdateState.Error(error.message ?: "Update check failed.")
            } finally {
                checking.set(false)
            }
        }
    }

    fun download(manifest: UpdateManifest) {
        if (downloadJob?.isActive == true) return
        downloadJob = scope.launch {
            val destination = File(updatesDirectory(), manifest.apk)
            try {
                require(manifest.versionCode.toLong() >= minInstallableCode()) { "This update is no longer newer than the installed app." }
                _state.value = UpdateState.Downloading(manifest, 0f)
                destination.delete()
                val digest = MessageDigest.getInstance("SHA-256")
                val expectedUrl = apkUrl(supportedAbi(), manifest.apk)
                val connection = open(expectedUrl)
                connection.inputStream.use { input ->
                    destination.outputStream().use { output ->
                        val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
                        var copied = 0L
                        while (true) {
                            val count = input.read(buffer)
                            if (count < 0) break
                            output.write(buffer, 0, count)
                            digest.update(buffer, 0, count)
                            copied += count
                            require(copied <= manifest.size) { "Downloaded update exceeds declared size." }
                            _state.value = UpdateState.Downloading(manifest, copied.toFloat() / manifest.size)
                        }
                        require(copied == manifest.size) { "Downloaded update size does not match manifest." }
                    }
                }
                connection.disconnect()
                require(digest.digest().toHex() == manifest.sha256) { "Downloaded update hash does not match manifest." }
                verifyApk(destination, manifest)
                _state.value = UpdateState.Ready(manifest, destination)
            } catch (_: CancellationException) {
                destination.delete()
                _state.value = UpdateState.Available(manifest)
            } catch (error: Exception) {
                destination.delete()
                _state.value = UpdateState.Error(error.message ?: "Update download failed.")
            } finally {
                downloadJob = null
            }
        }
    }

    fun cancelDownload() {
        downloadJob?.cancel()
    }

    fun install(manifest: UpdateManifest, apk: File) {
        if (!apk.isFile) {
            _state.value = UpdateState.Error("Downloaded update is no longer available.")
            return
        }
        if (!context.packageManager.canRequestPackageInstalls()) {
            _state.value = UpdateState.PermissionNeeded(manifest, apk)
            return
        }
        // Permission is ours — leave PermissionNeeded so a resume doesn't re-launch the installer in a loop.
        _state.value = UpdateState.Ready(manifest, apk)
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", apk)
        context.startActivity(
            Intent(Intent.ACTION_VIEW)
                .setDataAndType(uri, "application/vnd.android.package-archive")
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        )
    }

    fun requestInstallPermission() {
        context.startActivity(
            Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES)
                .setData(Uri.parse("package:${context.packageName}"))
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        )
    }

    private fun supportedAbi(): String = when {
        Build.SUPPORTED_ABIS.contains("arm64-v8a") -> "arm64-v8a"
        Build.SUPPORTED_ABIS.contains("x86_64") -> "x86_64"
        else -> error("This device architecture is not supported by Promtuz updates.")
    }

    private fun manifestUrl(abi: String) = "https://apt.promtuz.dev/apk/$channel/$abi/manifest.json"

    private fun apkUrl(abi: String, filename: String) =
        "https://apt.promtuz.dev/apk/$channel/$abi/$filename"

    private fun getBytes(url: String): ByteArray {
        val connection = open(url)
        return try {
            connection.inputStream.use { it.readBytes() }
        } finally {
            connection.disconnect()
        }
    }

    private fun open(url: String): HttpURLConnection {
        val parsed = URL(url)
        val abi = supportedAbi()
        val expectedPaths = setOf(
            "/apk/$channel/$abi/manifest.json",
            "/apk/$channel/$abi/manifest.json.sig",
        )
        val apkPrefix = "/apk/$channel/$abi/promtuz-"
        require(parsed.protocol == "https" && parsed.host == "apt.promtuz.dev") { "Update server is not trusted." }
        require((parsed.port == -1 || parsed.port == 443) && parsed.userInfo == null && parsed.query == null && parsed.ref == null) {
            "Update URL is invalid."
        }
        require(parsed.path in expectedPaths || (parsed.path.startsWith(apkPrefix) && parsed.path.endsWith(".apk"))) {
            "Update path is invalid."
        }
        return (parsed.openConnection() as HttpURLConnection).apply {
            instanceFollowRedirects = false
            connectTimeout = 15_000
            readTimeout = 30_000
            requestMethod = "GET"
            require(responseCode == HttpURLConnection.HTTP_OK) { "Update server returned HTTP $responseCode." }
        }
    }

    private fun validateManifest(manifest: UpdateManifest, abi: String) {
        require(manifest.versionCode > 0 && manifest.size > 0) { "Update manifest contains invalid version or size." }
        require(manifest.versionName.matches(Regex("[A-Za-z0-9][A-Za-z0-9._+-]*"))) { "Update manifest contains invalid version name." }
        require(manifest.apk == "promtuz-${manifest.versionName}~${manifest.versionCode}.apk") { "Update filename is invalid." }
        require(manifest.sha256.matches(Regex("[0-9a-f]{64}"))) { "Update manifest contains invalid hash." }
        require(URL(apkUrl(abi, manifest.apk)).path.endsWith("/${manifest.apk}")) { "Update path is invalid." }
    }

    private fun verifyApk(apk: File, manifest: UpdateManifest) {
        val flags = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            PackageManager.GET_SIGNING_CERTIFICATES
        } else {
            @Suppress("DEPRECATION") PackageManager.GET_SIGNATURES
        }
        val downloaded = context.packageManager.getPackageArchiveInfo(apk.path, flags)
            ?: error("Downloaded file is not an Android package.")
        require(downloaded.packageName == context.packageName) { "Downloaded package belongs to another app." }
        require(packageVersionCode(downloaded) == manifest.versionCode.toLong()) { "Downloaded package version does not match manifest." }
        require(signers(downloaded) == signers(installedPackage(flags))) { "Downloaded package signer does not match installed app." }
    }

    private fun installedPackage(flags: Int): PackageInfo = context.packageManager.getPackageInfo(context.packageName, flags)

    private fun signers(info: PackageInfo): Set<String> {
        val signatures = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            info.signingInfo?.apkContentsSigners
        } else {
            @Suppress("DEPRECATION") info.signatures
        } ?: emptyArray()
        return signatures.map { MessageDigest.getInstance("SHA-256").digest(it.toByteArray()).toHex() }.toSet()
    }

    private fun installedVersionCode(): Long = packageVersionCode(installedPackage(0))

    @Suppress("DEPRECATION")
    private fun packageVersionCode(info: PackageInfo): Long = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
        info.longVersionCode
    } else {
        info.versionCode.toLong()
    }

    private fun updatesDirectory(): File = File(context.cacheDir, "updates").apply { mkdirs() }
    private fun ByteArray.toHex(): String = joinToString("") { "%02x".format(Locale.ROOT, it) }
}
