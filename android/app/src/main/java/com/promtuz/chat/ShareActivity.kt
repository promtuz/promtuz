package com.promtuz.chat

import android.app.Application
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.foundation.Image
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.unit.dp
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import androidx.navigation3.runtime.NavBackStack
import androidx.navigation3.runtime.NavKey
import androidx.navigation3.runtime.entryProvider
import com.promtuz.chat.navigation.NavStage
import com.promtuz.chat.ui.appearance.AppearanceStore
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.theme.PromtuzTheme
import com.promtuz.chat.utils.extensions.*
import com.promtuz.chat.utils.media.*
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlinx.coroutines.*
import kotlinx.coroutines.flow.*
import kotlinx.serialization.Serializable

@Serializable private data object ShareChoose : NavKey
@Serializable private data class ShareReview(val conversation: String, val name: String) : NavKey

/** Holds incoming URI grants for the entire choose/review flow, including rotation. */
class ShareActivity : ComponentActivity() {
    private val model: IncomingShareVM by viewModels()
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
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
                } else NavStage(model.stack, onBack = { if (model.stack.size > 1) model.stack.removeLastOrNull() else finish() }, entryProvider = entryProvider {
                    entry<ShareChoose> { ChooseShareRecipient(model) }
                    entry<ShareReview> { key -> ReviewShare(model, key) { finish() } }
                })
                }
            }
        }
    }
}

data class ShareRecipient(val conversation: String, val name: String, val group: Boolean, val peer: String?)

private data class SharedPick(val id: ULong, val name: String, val preview: ImageBitmap?)
class IncomingShareVM(application: Application) : AndroidViewModel(application) {
    val stack = NavBackStack<NavKey>(ShareChoose)
    var text by mutableStateOf("")
    var query by mutableStateOf("")
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
    private val removing = mutableStateListOf<ULong>()
    private val cleanup = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    val staged = observeQuery(setOf("staging")) { CoreBridge.stagedItems() }
        .stateIn(viewModelScope, SharingStarted.Eagerly, emptyList())
    val recipients = observeQuery(setOf("contacts", "conversations", "conversation_members")) {
        val contacts = CoreBridge.contacts().associateBy { it.ipk.toHex() }
        CoreBridge.listConversations().filter { c ->
            if (c.kind.toInt() == 1) c.canSend else contacts[c.peer?.toHex()]?.status?.toInt() == 1
        }.map { c -> ShareRecipient(c.id.toHex(), if (c.kind.toInt() == 1) c.displayName else contacts[c.peer?.toHex()]?.name.orEmpty(), c.kind.toInt() == 1, c.peer?.toHex()) }
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(5000), emptyList())

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
        if (uris.any { it.scheme != "content" || it.authority == "${getApplication<Application>().packageName}.fileprovider" }) {
            error = "This app didn't provide a readable attachment"; return
        }
        if (uris.isEmpty()) { if (text.isBlank()) error = "Nothing to share"; return }
        busy = true
        importJob = viewModelScope.launch {
            try {
                for (uri in uris) {
                    ensureActive()
                    val context = getApplication<Application>()
                    val mime = context.contentResolver.getType(uri).orEmpty()
                    // Preparation stays cancellable; each helper protects just the core handoff.
                    // Record its returned ID without suspension before preparing the next item.
                    if (mime.startsWith("image/")) {
                        val image = stagePickedImage(context, uri)
                        owned.add(SharedPick(image.id, "Photo", image.preview))
                    } else {
                        val attachment = stagePickedAttachment(context, uri)
                        owned.add(SharedPick(attachment.id, attachment.name, attachment.preview))
                    }
                }
            } catch (e: CancellationException) { throw e }
            catch (e: Exception) {
                val reason = when (e) {
                    is ImagePreparationException -> e.message
                    is VideoPreparationException -> e.message
                    is uniffi.core.CoreException.Refused -> e.msg
                    else -> null
                }
                error = reason?.let { if (owned.isEmpty()) it else "$it Only the items shown will be sent." }
                    ?: "Some attachments couldn't be added. Only the items shown will be sent."
            }
            finally { busy = false }
        }
    }

    private fun remove(id: ULong) {
        if (busy || id in removing) return
        removing += id
        cleanup.launch(Dispatchers.Main.immediate) {
            try {
                withContext(NonCancellable) {
                    CoreBridge.discardStaged(id)
                    owned.removeAll { it.id == id }
                }
            } catch (_: Exception) {
                error = "Couldn’t remove the attachment. Try again."
            } finally { removing -= id }
        }
    }

    @Composable fun Picks() {
        val records by staged.collectAsState()
        owned.toList().forEach { pick ->
            val state = records.firstOrNull { it.id == pick.id }
            ListItem(leadingContent = { pick.preview?.let { Image(it, null, Modifier.size(48.dp)) } },
                supportingContent = { if (state?.state?.toInt() != 1) Text(if (state?.state?.toInt() == 2) "Couldn't prepare attachment" else "Preparing…") },
                trailingContent = { IconButton(enabled = !busy && pick.id !in removing, onClick = { remove(pick.id) }) { DrawableIcon(R.drawable.oi_trash, desc = "Remove attachment") } }) {
                Text(pick.name, maxLines = 2)
            }
        }
    }

    fun canSend(records: List<uniffi.core.StagedRecord>) = !busy && removing.isEmpty() && (text.isNotBlank() || owned.isNotEmpty()) &&
        owned.all { p -> records.any { it.id == p.id && it.state.toInt() == 1 } }

    fun send(conversation: String, done: () -> Unit) {
        if (!canSend(staged.value)) return
        busy = true; sending = true; error = null
        sendJob = viewModelScope.launch {
            try {
                if (owned.isEmpty()) withContext(NonCancellable) { CoreBridge.commitShared(conversation.fromHex(), emptyList(), text.trim()) }
                else {
                    for (chunk in owned.toList().chunked(10)) {
                        ensureActive()
                        withContext(NonCancellable) {
                            try { CoreBridge.commitShared(conversation.fromHex(), chunk.map { it.id }, text.trim()) }
                            finally {
                                val remaining = CoreBridge.stagedItems().map { it.id }.toSet()
                                val committed = chunk.map { it.id }.filterNot { it in remaining }.toSet()
                                if (committed.isNotEmpty()) text = ""
                                owned.removeAll { it.id in committed }
                            }
                        }
                    }
                }
                done()
            } catch (e: CancellationException) { throw e }
            catch (_: Exception) {
                if (owned.isEmpty() && text.isEmpty()) done()
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

@Composable private fun ChooseShareRecipient(model: IncomingShareVM) {
    val recipients by model.recipients.collectAsState()
    SimpleScreen({ Text("Share to…") }, connectionStatus = false) { padding ->
        Column(Modifier.fillMaxSize().padding(top = padding.calculateTopPadding())) {
            OutlinedTextField(model.query, { model.query = it }, Modifier.fillMaxWidth().padding(horizontal = 18.dp, vertical = 8.dp),
                label = { Text("Search chats") }, singleLine = true)
            LazyColumn(contentPadding = PaddingValues(bottom = padding.calculateBottomPadding() + 16.dp)) {
                if (recipients.isEmpty()) item { Text("No chats yet", Modifier.padding(24.dp)) }
                items(recipients.filter { it.name.contains(model.query, ignoreCase = true) }, key = { it.conversation }) { (conv, name, group, peer) ->
                    ListItem(modifier = Modifier.clickable { model.stack.add(ShareReview(conv, name)) },
                        leadingContent = { if (group) GroupAvatar(name, emptyList(), conversation = conv) else Avatar(name, identityKey = peer ?: conv, image = rememberAvatar(peer)) },
                    ) { Text(name) }
                }
            }
        }
    }
}

@Composable private fun ReviewShare(model: IncomingShareVM, destination: ShareReview, done: () -> Unit) {
    val staged by model.staged.collectAsState()
    androidx.activity.compose.BackHandler(model.sending) { }
    SimpleScreen(title = { Text(destination.name) }, connectionStatus = false, actions = {
        TextButton(enabled = model.canSend(staged), onClick = { model.send(destination.conversation, done) }) { Text("Send") }
    }) { padding ->
        LazyColumn(Modifier.fillMaxSize().imePadding(), contentPadding = PaddingValues(start = 18.dp, end = 18.dp,
            top = padding.calculateTopPadding() + 16.dp, bottom = padding.calculateBottomPadding() + 24.dp)) {
            item { model.Picks() }
            item { OutlinedTextField(model.text, { model.text = it }, Modifier.fillMaxWidth().padding(vertical = 12.dp),
                enabled = !model.busy, minLines = 3, maxLines = 8, label = { Text("Message") }) }
            if (model.busy) item { LinearProgressIndicator(Modifier.fillMaxWidth()) }
            model.error?.let { item { Text(it, color = MaterialTheme.colorScheme.error) } }
        }
    }
}
