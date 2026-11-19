package com.promtuz.chat

import com.promtuz.chat.security.ScreenSecurity
import com.promtuz.chat.ui.components.TextButton
import android.app.Application
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.util.Patterns
import com.promtuz.chat.ui.screens.SendToScreen
import com.promtuz.chat.ui.screens.SendToSubject
import com.promtuz.chat.ui.screens.SubjectIconTile
import com.promtuz.chat.ui.screens.SubjectThumbStack
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.unit.dp
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.ui.appearance.AppearanceStore
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.theme.PromtuzTheme
import com.promtuz.chat.utils.extensions.*
import com.promtuz.chat.utils.media.*
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*

/** Holds incoming URI grants for the entire choose/review flow, including rotation. */
class ShareActivity : ComponentActivity() {
    private val model: IncomingShareVM by viewModels()
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        ScreenSecurity.apply(window)
        enableEdgeToEdge()
        model.receive(intent)
        setContent {
            val appearance by AppearanceStore.appearance.collectAsState()
            PromtuzTheme(appearance = appearance) {
                Box(Modifier.fillMaxSize()) {
                if (!CoreBridge.shouldLaunchApp()) {
                    SimpleScreen({ Text("Share to Promtuz") }) { padding -> Column(Modifier.padding(padding).padding(24.dp)) {
                        Text("Set up Promtuz before sharing.")
                        TextButton(onClick = { startActivity(Intent(this@ShareActivity, LauncherActivity::class.java)); finish() }) { Text("Open Promtuz") }
                    } }
                } else ShareScreen(model) { finish() }
                }
            }
        }
    }
}

internal data class SharedPick(val id: ULong, val name: String, val preview: ImageBitmap?)
class IncomingShareVM(application: Application) : AndroidViewModel(application) {
    var text by mutableStateOf("")
    /** A shared link stays whole on the bar; [text] is then the comment. */
    var link by mutableStateOf<String?>(null)
    val selected = mutableStateListOf<String>()
    var busy by mutableStateOf(false)
        private set
    var sending by mutableStateOf(false)
        private set
    var error by mutableStateOf<String?>(null)
        private set
    private var received = false
    private var importJob: Job? = null
    private var sendJob: Job? = null
    private val owned = mutableStateListOf<SharedPick>()
    private val cleanup = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    val staged = observeQuery(setOf("staging")) { CoreBridge.stagedItems() }
        .stateIn(viewModelScope, SharingStarted.Eagerly, emptyList())
    val targets = com.promtuz.chat.ui.screens.sendTargets()
        .stateIn(viewModelScope, SharingStarted.WhileSubscribed(5000), emptyList())

    fun receive(intent: Intent) {
        if (received) return
        received = true
        if (intent.action !in listOf(Intent.ACTION_SEND, Intent.ACTION_SEND_MULTIPLE)) { error = "Nothing to share"; return }
        text = intent.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString().orEmpty()
        @Suppress("DEPRECATION")
        val streams = if (intent.action == Intent.ACTION_SEND_MULTIPLE) intent.getParcelableArrayListExtra<Uri>(Intent.EXTRA_STREAM).orEmpty()
            else listOfNotNull(intent.getParcelableExtra<Uri>(Intent.EXTRA_STREAM))
        val clips = intent.clipData?.let { clip -> (0 until clip.itemCount).mapNotNull { clip.getItemAt(it).uri } }.orEmpty()
        val uris = (streams + clips).distinct()
        // Our own providers are never a share source: one holds unlocked private media.
        val own = getApplication<Application>().packageName.let { setOf("$it.fileprovider", "$it.private-media") }
        if (uris.any { it.scheme != "content" || it.authority in own }) {
            error = "This app didn't provide a readable attachment"; return
        }
        if (uris.isEmpty()) {
            if (text.isBlank()) error = "Nothing to share"
            else if (text.trim().let { it.none(Char::isWhitespace) && Patterns.WEB_URL.matcher(it).matches() }) { link = text.trim(); text = "" }
            return
        }
        busy = true
        importJob = viewModelScope.launch {
            try {
                for (uri in uris) {
                    ensureActive()
                    val context = getApplication<Application>()
                    // Content inspection decides whether this is image, video, or a raw file.
                    val attachment = stagePickedAttachment(context, uri)
                    owned.add(SharedPick(attachment.id, attachment.name, attachment.preview))
                }
            } catch (e: CancellationException) { throw e }
            catch (e: Exception) {
                val reason = when (e) {
                    is MediaPreparationException -> e.message
                    is uniffi.core.CoreException.Refused -> e.msg
                    else -> null
                }
                error = reason?.let { if (owned.isEmpty()) it else "$it Only the items shown will be sent." }
                    ?: "Some attachments couldn't be added. Only the items shown will be sent."
            }
            finally { busy = false }
        }
    }

    internal val picks: List<SharedPick> get() = owned

    fun canSend(records: List<uniffi.core.StagedRecord>) = !busy && selected.isNotEmpty() &&
        (text.isNotBlank() || link != null || owned.isNotEmpty()) &&
        owned.all { p -> records.any { it.id == p.id && it.state.toInt() == 1 } }

    fun send(done: () -> Unit) {
        if (!canSend(staged.value)) return
        val to = selected.map { it.fromHex() }
        busy = true; sending = true; error = null
        sendJob = viewModelScope.launch {
            // Chats that got nothing; a retry goes only to them.
            val failed = mutableSetOf<String>()
            try {
                if (owned.isEmpty()) withContext(NonCancellable) {
                    failed += CoreBridge.commitShared(to, emptyList(), listOfNotNull(text.trim().ifEmpty { null }, link).joinToString("\n"))
                        .map { it.toHex() }
                }
                else {
                    for (chunk in owned.toList().chunked(10)) {
                        ensureActive()
                        withContext(NonCancellable) {
                            try { failed += CoreBridge.commitShared(to, chunk.map { it.id }, text.trim()).map { it.toHex() } }
                            finally {
                                val remaining = CoreBridge.stagedItems().map { it.id }.toSet()
                                val committed = chunk.map { it.id }.filterNot { it in remaining }.toSet()
                                if (committed.isNotEmpty()) text = ""
                                owned.removeAll { it.id in committed }
                            }
                        }
                    }
                }
                if (failed.isEmpty()) done()
                else {
                    selected.retainAll(failed)
                    error = if (failed.size == 1) "Couldn't send to one chat. Try again." else "Couldn't send to ${failed.size} chats. Try again."
                }
            } catch (e: CancellationException) { throw e }
            catch (_: Exception) {
                if (owned.isEmpty() && text.isEmpty() && link == null) done()
                else error = "Couldn't send everything. You can retry the remaining items."
            } finally { busy = false; sending = false }
        }
    }

    override fun onCleared() {
        importJob?.cancel()
        sendJob?.cancel()
        cleanup.launch(Dispatchers.Main.immediate) {
            try {
                importJob?.join()
                sendJob?.join()
                owned.map { it.id }.forEach { runCatching { CoreBridge.discardStaged(it) } }
            } finally { cleanup.cancel() }
        }
        super.onCleared()
    }
}

@Composable private fun ShareScreen(model: IncomingShareVM, done: () -> Unit) {
    val targets by model.targets.collectAsState()
    val staged by model.staged.collectAsState()
    val picks = model.picks
    val subject = when {
        picks.isNotEmpty() -> SendToSubject(if (model.busy && !model.sending) "Preparing…" else describe(picks, staged)) {
            SubjectThumbStack(picks.map { it.preview })
        }
        model.link != null -> SendToSubject(model.link!!.removePrefix("https://").removePrefix("http://")) { SubjectIconTile(R.drawable.oi_link) }
        else -> SendToSubject("A message") { SubjectIconTile(R.drawable.oi_message_text) }
    }
    SendToScreen(
        title = "Send to…",
        targets = targets,
        selected = model.selected,
        onToggle = { if (!model.selected.remove(it)) model.selected.add(it) },
        subject = subject,
        comment = model.text,
        onComment = { model.text = it },
        commentHint = if (picks.isNotEmpty()) "Add a caption" else if (model.link != null) "Add a comment" else "Message",
        canSend = model.canSend(staged),
        busy = model.sending,
        error = model.error,
    ) { model.send(done) }
}

private fun describe(picks: List<SharedPick>, staged: List<uniffi.core.StagedRecord>): String {
    val kinds = picks.map { p -> staged.firstOrNull { it.id == p.id } }
    val photos = kinds.count { it?.kind?.toInt() == 1 }
    val videos = kinds.count { it?.kind?.toInt() != 1 && it?.mime?.startsWith("video/") == true }
    val files = picks.size - photos - videos
    fun n(count: Int, one: String, many: String) = if (count == 1) "a $one" else "$count $many"
    val parts = listOfNotNull(
        n(photos, "photo", "photos").takeIf { photos > 0 },
        n(videos, "video", "videos").takeIf { videos > 0 },
        n(files, "file", "files").takeIf { files > 0 },
    )
    val text = if (parts.size > 1) parts.dropLast(1).joinToString(", ") + " and " + parts.last() else parts.firstOrNull().orEmpty()
    return text.replaceFirstChar { it.uppercase() }
}
