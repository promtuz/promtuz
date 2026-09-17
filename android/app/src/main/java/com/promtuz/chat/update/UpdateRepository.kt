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
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import timber.log.Timber
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.Locale

sealed interface UpdateState {
    data object Unchecked : UpdateState
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
) {
    /** The subset core validates. `publishedAt` is display-only, so it stays here. */
    fun toCore() = uniffi.core.UpdateManifest(
        versionCode = versionCode.toUInt(),
        versionName = versionName,
        apk = apk,
        size = size.toULong(),
        sha256 = sha256,
    )
}

class UpdateRepository(private val context: Context) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var checkJob: Job? = null
    private val notificationCheck = Mutex()
    private val notifier = UpdateNotifier(context)
    private var downloadJob: Job? = null
    private var channelGeneration = 0L
    private val json = Json { ignoreUnknownKeys = false; isLenient = false }
    private val _state = MutableStateFlow<UpdateState>(UpdateState.Unchecked)
    val state: StateFlow<UpdateState> = _state.asStateFlow()
    private var screenVisible = false

    companion object {
        internal val CHANNELS = listOf("debug", "release")
        private const val TAG = "AppUpdater"
        private val log = { Timber.tag(TAG) }
    }

    private val nativeChannel = if (context.applicationInfo.flags and android.content.pm.ApplicationInfo.FLAG_DEBUGGABLE != 0) {
        "debug"
    } else {
        "release"
    }
    private val _selectedChannel = MutableStateFlow(ChatPrefs.updateChannel?.takeIf { it in CHANNELS } ?: nativeChannel)
    val selectedChannel: StateFlow<String> = _selectedChannel.asStateFlow()
    val channel: String get() = _selectedChannel.value

    init {
        notifier.clearIfInstalled(installedVersionCode(), nativeChannel)
    }

    /** Cross-channel switch: drop any in-flight/staged update from the old channel, then re-check. */
    fun switchChannel(newChannel: String) {
        require(newChannel in CHANNELS)
        if (newChannel == channel) return
        channelGeneration++
        ChatPrefs.updateChannel = newChannel
        _selectedChannel.value = newChannel
        notifier.clear()
        checkJob?.cancel()
        checkJob = null
        downloadJob?.cancel()
        _state.value = UpdateState.Unchecked
        UpdateWorker.enqueue(context, replace = true)
        check()
    }

    // Same-versionCode installs are allowed when crossing channels (the binaries
    // differ); the OS rejects downgrades either way.
    private fun installable(manifest: UpdateManifest, selectedChannel: String = channel): Boolean =
        CoreBridge.updateIsInstallable(
            manifest.versionCode, installedVersionCode(), selectedChannel != nativeChannel,
        )

    fun setScreenVisible(visible: Boolean) {
        screenVisible = visible
        if (!visible) return
        notifier.clear()
        val manifest = when (val state = _state.value) {
            is UpdateState.Available -> state.manifest
            is UpdateState.Downloading -> state.manifest
            is UpdateState.Ready -> state.manifest
            is UpdateState.PermissionNeeded -> state.manifest
            else -> null
        }
        if (manifest != null) notifier.markSeen(manifest, channel)
    }

    /** A push is only a hint. The signed manifest decides what we can offer. */
    internal suspend fun checkAndNotify() = withContext(Dispatchers.IO) {
        notificationCheck.withLock {
            val selectedChannel = channel
            val manifest = availableUpdate(selectedChannel)
            currentCoroutineContext().ensureActive()
            withContext(Dispatchers.Main.immediate) notify@{
                // Channel switches and screen actions run on main too.
                if (selectedChannel != channel) return@notify
                if (manifest == null || screenVisible || _state.value is UpdateState.Downloading ||
                    _state.value is UpdateState.Ready || _state.value is UpdateState.PermissionNeeded) {
                    notifier.clear()
                } else {
                    notifier.show(manifest, selectedChannel)
                }
            }
        }
    }

    private fun availableUpdate(selectedChannel: String): UpdateManifest? {
        val abi = supportedAbi()
        val url = "https://apt.promtuz.dev/apk/$selectedChannel/$abi/manifest.json"
        val rawManifest = getBytes(url, selectedChannel, 16 * 1024)
        val signature = getBytes("$url.sig", selectedChannel, 64)
        require(CoreBridge.verifyUpdateManifest(rawManifest, signature)) { "Update signature could not be verified." }
        val manifest = json.decodeFromString<UpdateManifest>(rawManifest.decodeToString())
        validateManifest(manifest, abi, selectedChannel)
        return manifest.takeIf { installable(it, selectedChannel) }
    }

    fun check() {
        // A foreground auto-check must not stomp an update the user is already
        // downloading or about to install — the verified APK is on disk; don't send them back to "Download".
        when (_state.value) {
            is UpdateState.Downloading, is UpdateState.Ready, is UpdateState.PermissionNeeded -> return
            else -> {}
        }
        if (checkJob?.isActive == true) return
        val previousDownload = downloadJob
        checkJob = scope.launch {
            val selectedChannel = channel
            val generation = channelGeneration
            try {
                log().v("Checking for Updates...")
                _state.value = UpdateState.Checking
                // A channel change may still be closing/deleting the previous download.
                previousDownload?.join()
                val manifest = withContext(Dispatchers.IO) { availableUpdate(selectedChannel) }
                if (generation == channelGeneration) {
                    _state.value = manifest?.let { UpdateState.Available(it) } ?: UpdateState.None
                    if (manifest != null && screenVisible) notifier.markSeen(manifest, selectedChannel)
                    if (manifest == null || screenVisible) notifier.clear()
                }
            } catch (error: CancellationException) {
                throw error
            } catch (error: Exception) {
                if (generation == channelGeneration) {
                    _state.value = UpdateState.Error(error.message ?: "Update check failed.")
                }
            }
        }
    }

    fun download(manifest: UpdateManifest) {
        if (downloadJob?.isCompleted == false) return
        val selectedChannel = channel
        val generation = channelGeneration
        notifier.clear()
        notifier.markSeen(manifest, selectedChannel)
        downloadJob = scope.launch {
            val destination = File(updatesDirectory(), manifest.apk)
            try {
                require(installable(manifest)) { "This update is no longer newer than the installed app." }
                _state.value = UpdateState.Downloading(manifest, 0f)
                withContext(Dispatchers.IO) {
                    destination.delete()
                    val digest = MessageDigest.getInstance("SHA-256")
                    val expectedUrl = apkUrl(supportedAbi(), manifest.apk, selectedChannel)
                    val connection = open(expectedUrl, selectedChannel)
                    try {
                        connection.inputStream.use { input ->
                            destination.outputStream().use { output ->
                                val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
                                var copied = 0L
                                var lastProgress = 0L
                                while (true) {
                                    currentCoroutineContext().ensureActive()
                                    val count = input.read(buffer)
                                    if (count < 0) break
                                    output.write(buffer, 0, count)
                                    digest.update(buffer, 0, count)
                                    copied += count
                                    require(copied <= manifest.size) { "Downloaded update exceeds declared size." }
                                    val now = System.nanoTime()
                                    if (now - lastProgress >= 50_000_000 || copied == manifest.size) {
                                        withContext(Dispatchers.Main.immediate) {
                                            _state.value = UpdateState.Downloading(manifest, copied.toFloat() / manifest.size)
                                        }
                                        lastProgress = now
                                    }
                                }
                                require(copied == manifest.size) { "Downloaded update size does not match manifest." }
                            }
                        }
                    } finally {
                        connection.disconnect()
                    }
                    require(digest.digest().toHex() == manifest.sha256) { "Downloaded update hash does not match manifest." }
                    verifyApk(destination, manifest)
                }
                _state.value = UpdateState.Ready(manifest, destination)
            } catch (_: CancellationException) {
                destination.delete()
                if (generation == channelGeneration) _state.value = UpdateState.Available(manifest)
            } catch (error: Exception) {
                destination.delete()
                if (generation == channelGeneration) _state.value = UpdateState.Error(error.message ?: "Update download failed.")
            } finally {
                if (downloadJob === currentCoroutineContext()[Job]) downloadJob = null
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

    private fun apkUrl(abi: String, filename: String, selectedChannel: String) =
        "https://apt.promtuz.dev/apk/$selectedChannel/$abi/$filename"

    private fun getBytes(url: String, selectedChannel: String, limit: Int): ByteArray {
        val connection = open(url, selectedChannel)
        return try {
            connection.inputStream.use { input ->
                val output = java.io.ByteArrayOutputStream()
                val buffer = ByteArray(1024)
                while (true) {
                    val count = input.read(buffer)
                    if (count < 0) break
                    require(output.size() + count <= limit) { "Update response is too large." }
                    output.write(buffer, 0, count)
                }
                output.toByteArray()
            }
        } finally {
            connection.disconnect()
        }
    }

    private fun open(url: String, selectedChannel: String): HttpURLConnection {
        val parsed = URL(url)
        val abi = supportedAbi()
        val expectedPaths = setOf(
            "/apk/$selectedChannel/$abi/manifest.json",
            "/apk/$selectedChannel/$abi/manifest.json.sig",
        )
        val apkPrefix = "/apk/$selectedChannel/$abi/promtuz-"
        require(parsed.protocol == "https" && parsed.host == "apt.promtuz.dev") { "Update server is not trusted." }
        require((parsed.port == -1 || parsed.port == 443) && parsed.userInfo == null && parsed.query == null && parsed.ref == null) {
            "Update URL is invalid."
        }
        require(parsed.path in expectedPaths || (parsed.path.startsWith(apkPrefix) && parsed.path.endsWith(".apk"))) {
            "Update path is invalid."
        }
        return (parsed.openConnection() as HttpURLConnection).apply {
            instanceFollowRedirects = false
            useCaches = false
            connectTimeout = 15_000
            readTimeout = 30_000
            requestMethod = "GET"
            try {
                require(responseCode == HttpURLConnection.HTTP_OK) { "Update server returned HTTP $responseCode." }
            } catch (error: Exception) {
                disconnect()
                throw error
            }
        }
    }

    /**
     * Check the manifest against the contract before anything is downloaded.
     * The contract is the server's, not Android's, so core states it — every
     * field here arrives over the network and the filename names what we fetch.
     */
    private fun validateManifest(manifest: UpdateManifest, abi: String, selectedChannel: String) {
        CoreBridge.validateUpdateManifest(manifest.toCore())
        require(URL(apkUrl(abi, manifest.apk, selectedChannel)).path.endsWith("/${manifest.apk}")) { "Update path is invalid." }
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
