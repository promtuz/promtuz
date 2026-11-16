package com.promtuz.chat.update

import android.content.Context
import android.content.Intent
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.Settings
import androidx.core.content.FileProvider
import com.promtuz.chat.BuildConfig
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
    data class Ready(val manifest: UpdateManifest, val apk: File, val needsInstallPermission: Boolean = false) : UpdateState
    data class Error(val message: String) : UpdateState
}

val UpdateState.offered: UpdateManifest?
    get() = when (this) {
        is UpdateState.Available -> manifest
        is UpdateState.Downloading -> manifest
        is UpdateState.Ready -> manifest
        else -> null
    }

/** [body] is a small Markdown subset. */
class ReleaseNote(val code: Long, val version: String, val date: String, val body: String) {
    companion object {
        private val HEADER = Regex("""^# (\S+) - (\d{4}-\d{2}-\d{2})\n?""")

        /** The release tool writes the first line as `# <version> - <date>`. */
        fun parse(code: Long, text: String): ReleaseNote {
            val header = HEADER.find(text)
            return ReleaseNote(
                code,
                header?.groupValues?.get(1) ?: "Build $code",
                header?.groupValues?.get(2).orEmpty(),
                text.substring(header?.range?.let { it.last + 1 } ?: 0).trim(),
            )
        }
    }
}

private class HttpError(val status: Int) : RuntimeException("Update server returned HTTP $status.")

@Serializable
data class UpdateManifest(
    val versionCode: Int,
    val versionName: String,
    val apk: String,
    val sha256: String,
    val size: Long,
    val publishedAt: String,
) {
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
    private var installedNotesJob: Job? = null
    private val notificationCheck = Mutex()
    private val notifier = UpdateNotifier(context)
    private var downloadJob: Job? = null
    private var channelGeneration = 0L
    private val json = Json { ignoreUnknownKeys = false; isLenient = false }
    private val _state = MutableStateFlow<UpdateState>(UpdateState.Unchecked)
    val state: StateFlow<UpdateState> = _state.asStateFlow()
    private val _notes = MutableStateFlow<List<ReleaseNote>>(emptyList())
    val notes: StateFlow<List<ReleaseNote>> = _notes.asStateFlow()
    /** Set when the installed build can no longer be used; the app is gated on it. */
    private val _required = MutableStateFlow<UpdateManifest?>(null)
    val required: StateFlow<UpdateManifest?> = _required.asStateFlow()

    private class Offer(val manifest: UpdateManifest, val notes: List<ReleaseNote>)
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
    private val notesStore = ReleaseNotesStore(context, installedVersionCode(), BuildConfig.VERSION_NAME, nativeChannel)
    // A Studio install can replace a release build without touching account preferences.
    // Follow the newly installed build; later explicit choices remain valid until another switch.
    private val initialChannel = if (notesStore.channelChanged) {
        ChatPrefs.updateChannel = null
        nativeChannel
    } else ChatPrefs.updateChannel?.takeIf { it in CHANNELS } ?: nativeChannel
    val installedChannel: String get() = nativeChannel
    private val _history = MutableStateFlow<List<ReleaseNote>>(emptyList())
    val history: StateFlow<List<ReleaseNote>> = _history.asStateFlow()
    private val _historyLoading = MutableStateFlow(false)
    val historyLoading: StateFlow<Boolean> = _historyLoading.asStateFlow()
    private val _historyError = MutableStateFlow<String?>(null)
    val historyError: StateFlow<String?> = _historyError.asStateFlow()
    private var historyJob: Job? = null
    private val _selectedChannel = MutableStateFlow(initialChannel)
    val selectedChannel: StateFlow<String> = _selectedChannel.asStateFlow()
    val channel: String get() = _selectedChannel.value

    init {
        if (notesStore.channelChanged) notifier.clear()
        else notifier.clearIfInstalled(installedVersionCode(), nativeChannel)
        refreshInstalledNotes()
    }

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
        accept(null)
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
        _state.value.offered?.let { notifier.markSeen(it, channel) }
    }

    private fun isRequired(manifest: UpdateManifest) =
        manifest.versionCode.toLong() > installedVersionCode() && CoreBridge.updateIsRequired(BuildConfig.VERSION_NAME, manifest.versionName)

    private fun accept(offer: Offer?) {
        _notes.value = offer?.notes.orEmpty()
        _required.value = offer?.manifest?.takeIf(::isRequired)
    }

    /** A push is only a hint. The signed manifest decides what we can offer. */
    internal suspend fun checkAndNotify() = withContext(Dispatchers.IO) {
        notificationCheck.withLock {
            val selectedChannel = channel
            val offer = availableUpdate(selectedChannel)
            currentCoroutineContext().ensureActive()
            withContext(Dispatchers.Main.immediate) notify@{
                // Channel switches and screen actions run on main too.
                if (selectedChannel != channel) return@notify
                accept(offer)
                if (offer == null || screenVisible || _state.value is UpdateState.Downloading ||
                    _state.value is UpdateState.Ready) {
                    notifier.clear()
                } else {
                    notifier.show(offer.manifest, selectedChannel, isRequired(offer.manifest),
                        channelSwitch = offer.manifest.versionCode.toLong() == installedVersionCode() && selectedChannel != nativeChannel)
                }
            }
        }
    }

    private fun availableUpdate(selectedChannel: String): Offer? {
        val abi = supportedAbi()
        val url = "https://apt.promtuz.dev/apk/$selectedChannel/$abi/manifest.json"
        val rawManifest = getBytes(url, selectedChannel, 16 * 1024)
        val signature = getBytes("$url.sig", selectedChannel, 64)
        require(CoreBridge.verifyUpdateManifest(rawManifest, signature)) { "Update signature could not be verified." }
        val manifest = json.decodeFromString<UpdateManifest>(rawManifest.decodeToString())
        validateManifest(manifest, abi, selectedChannel)
        if (!installable(manifest, selectedChannel)) return null
        return Offer(manifest, releaseNotes(selectedChannel, abi, manifest))
    }

    private suspend fun refreshNoteViews() {
        val notes = withContext(Dispatchers.IO) {
            notesStore.cached(nativeChannel).filter { it.code <= notesStore.code }
        }
        _history.value = notes
    }

    private fun readNote(selectedChannel: String, abi: String, code: Long): ReleaseNote? {
        notesStore.read(selectedChannel, code)?.let { return it }
        return try {
            val url = "https://apt.promtuz.dev/apk/$selectedChannel/$abi/notes-$code.md"
            notesStore.write(selectedChannel, code, getBytes(url, selectedChannel, 64 * 1024),
                getBytes("$url.sig", selectedChannel, 64))
        } catch (error: HttpError) {
            if (error.status == 404) null else throw error
        }
    }

    private fun noteCodes(selectedChannel: String, abi: String, after: Long, through: Long): List<Long> {
        val url = "https://apt.promtuz.dev/apk/$selectedChannel/$abi/notes-index.json"
        val codes = try {
            val raw = getBytes(url, selectedChannel, 256 * 1024)
            val signature = getBytes("$url.sig", selectedChannel, 64)
            require(CoreBridge.verifyUpdateManifest(raw, signature)) { "Notes index signature could not be verified." }
            json.decodeFromString<List<Long>>(raw.decodeToString())
        } catch (error: HttpError) {
            if (error.status != 404) throw error
            // Compatibility with releases published before the index existed.
            return (through downTo maxOf(after + 1, through - 199)).toList()
        }
        require(codes.all { it in 1..2_100_000_000L }) { "Invalid release notes index." }
        return codes.distinct().filter { it > after && it <= through }.sortedDescending()
    }

    /** Notes failure must not hide an otherwise valid update. Verified notes survive installation. */
    private fun releaseNotes(selectedChannel: String, abi: String, offered: UpdateManifest): List<ReleaseNote> {
        val installed = installedVersionCode()
        val after = if (offered.versionCode.toLong() == installed) installed - 1 else installed
        try {
            noteCodes(selectedChannel, abi, after, offered.versionCode.toLong()).forEach { code ->
                readNote(selectedChannel, abi, code)
            }
        } catch (error: Exception) {
            log().w(error, "Release notes unavailable")
        }
        return notesStore.cached(selectedChannel).filter { it.code > after && it.code <= offered.versionCode }
    }

    private fun refreshInstalledNotes() {
        if (installedNotesJob?.isActive == true) return
        installedNotesJob = scope.launch {
            refreshNoteViews()
            try {
                withContext(Dispatchers.IO) {
                    val after = notesStore.previousCode ?: (notesStore.code - 1)
                    noteCodes(nativeChannel, supportedAbi(), after, notesStore.code).forEach { code ->
                        readNote(nativeChannel, supportedAbi(), code)
                    }
                }
            } catch (error: CancellationException) {
                throw error
            } catch (error: Exception) {
                log().w(error, "Installed release notes unavailable")
            }
            refreshNoteViews()
        }
    }

    fun loadHistory() {
        if (historyJob?.isActive == true) return
        historyJob = scope.launch {
            _historyLoading.value = true
            _historyError.value = null
            try {
                withContext(Dispatchers.IO) {
                    noteCodes(nativeChannel, supportedAbi(), 0, notesStore.code).forEach { code ->
                        readNote(nativeChannel, supportedAbi(), code)
                    }
                }
            } catch (error: CancellationException) {
                throw error
            } catch (error: Exception) {
                _historyError.value = "Couldn't load release notes."
            } finally {
                refreshNoteViews()
                _historyLoading.value = false
            }
        }
    }

    fun check() {
        refreshInstalledNotes()
        // A foreground check must not replace a downloaded update or interrupt a download.
        when (_state.value) {
            is UpdateState.Downloading, is UpdateState.Ready -> return
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
                val offer = withContext(Dispatchers.IO) { availableUpdate(selectedChannel) }
                if (generation == channelGeneration) {
                    accept(offer)
                    _state.value = offer?.let { UpdateState.Available(it.manifest) } ?: UpdateState.None
                    if (offer != null && screenVisible) notifier.markSeen(offer.manifest, selectedChannel)
                    if (offer == null || screenVisible) notifier.clear()
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
            _state.value = UpdateState.Ready(manifest, apk, needsInstallPermission = true)
            return
        }
        _state.value = UpdateState.Ready(manifest, apk)
        try {
            val uri = FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", apk)
            context.startActivity(
                Intent(Intent.ACTION_VIEW)
                    .setDataAndType(uri, "application/vnd.android.package-archive")
                    .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
            )
        } catch (error: Exception) {
            log().w(error, "Update installer did not open")
            _state.value = UpdateState.Error("Couldn't open the installer. Try again.")
        }
    }

    fun dismissInstallPermission() {
        (_state.value as? UpdateState.Ready)?.let { _state.value = it.copy(needsInstallPermission = false) }
    }

    fun requestInstallPermission() {
        dismissInstallPermission()
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
            "/apk/$selectedChannel/$abi/notes-index.json",
            "/apk/$selectedChannel/$abi/notes-index.json.sig",
        )
        val apkPrefix = "/apk/$selectedChannel/$abi/promtuz-"
        val notes = Regex("/apk/$selectedChannel/$abi/notes-[0-9]{1,10}\\.md(\\.sig)?")
        require(parsed.protocol == "https" && parsed.host == "apt.promtuz.dev") { "Update server is not trusted." }
        require((parsed.port == -1 || parsed.port == 443) && parsed.userInfo == null && parsed.query == null && parsed.ref == null) {
            "Update URL is invalid."
        }
        require(parsed.path in expectedPaths || notes.matches(parsed.path) || (parsed.path.startsWith(apkPrefix) && parsed.path.endsWith(".apk"))) {
            "Update path is invalid."
        }
        return (parsed.openConnection() as HttpURLConnection).apply {
            instanceFollowRedirects = false
            useCaches = false
            connectTimeout = 15_000
            readTimeout = 30_000
            requestMethod = "GET"
            try {
                if (responseCode != HttpURLConnection.HTTP_OK) throw HttpError(responseCode)
            } catch (error: Exception) {
                disconnect()
                throw error
            }
        }
    }

    /** Runs before any download: every field arrives over the network, and the filename names what we fetch. */
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
