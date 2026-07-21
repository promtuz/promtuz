package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.net.Uri
import android.os.SystemClock
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.domain.model.Activity
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.Presence
import com.promtuz.chat.domain.model.Quote
import com.promtuz.chat.domain.model.ReactionGroup
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.chat.utils.media.decodeAvifCached
import com.promtuz.chat.utils.media.decodeDownscaled
import com.promtuz.chat.utils.media.resolvePickedFile
import com.promtuz.chat.utils.media.toRgba
import com.promtuz.core.CoreBridge
import com.promtuz.core.observeQuery
import kotlin.random.Random
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.launch
import uniffi.core.MediaRecord
import uniffi.core.MessageRecord
import uniffi.core.ReactionRecord

/**
 * Reactive chat. [messages] observes the DB — re-read on every commit touching
 * messages/reactions — so send / receive / edit / delete / reaction / receipt all
 * surface as row updates with no hand-patching. [input] is the draft, cleared the
 * instant [send] fires (so the editor empties immediately). Newest message sits at
 * index 0 and the list draws reversed, so new messages land at the bottom. Typing
 * is an ephemeral signal, timed out client-side.
 */
class ChatVM(private val application: Application) : ViewModel() {
    private var peer: ByteArray = ByteArray(32)
    private var started = false

    private val _messages = MutableStateFlow<List<UiMessage>>(emptyList())
    val messages: StateFlow<List<UiMessage>> = _messages.asStateFlow()

    /** Composer draft; two-way bound to the input field, cleared on [send]. */
    val input = MutableStateFlow("")

    /** Reply/edit staging shown as a chip above the composer; consumed by [send]. */
    val composerAction = MutableStateFlow<ComposerAction?>(null)

    private val _typing = MutableStateFlow(false)
    val typing: StateFlow<Boolean> = _typing.asStateFlow()
    private var typingExpiry: Job? = null

    /** Key of the incoming message that ended a live typing signal — the morph target. */
    val typingHandoff = MutableStateFlow<String?>(null)

    private val _presence = MutableStateFlow<Presence?>(null)
    val presence: StateFlow<Presence?> = _presence.asStateFlow()

    fun init(peerIpk: ByteArray) {
        if (started) return
        started = true
        peer = peerIpk

        var newestIncoming: String? = null
        viewModelScope.launch {
            observeQuery(setOf("messages", "reactions", "message_media", "partials")) { load() }.collect { list ->
                // Their message just landed — if they were typing, it inherits the
                // typing bubble (morph), and with this chat on screen it's read:
                // receipt the high-water mark. Handoff is set BEFORE the list so
                // one recomposition sees both.
                val newest = list.firstOrNull { !it.outgoing }
                if (newest?.key != newestIncoming) {
                    newestIncoming = newest?.key
                    if (_typing.value && newest != null) typingHandoff.value = newest.key
                    clearTyping()
                    newest?.dispatchIdHex?.let { did ->
                        fire { CoreBridge.markRead(peer, did.fromHex()) }
                    }
                }
                _messages.value = list
            }
        }

        viewModelScope.launch {
            CoreBridge.activity.filter { it.peer.contentEquals(peer) }.collect { sig ->
                if (Activity.Typing in Activity.fromBits(sig.bits)) {
                    _typing.value = true
                    typingExpiry?.cancel()
                    typingExpiry = viewModelScope.launch { delay(TYPING_TTL_MS); _typing.value = false }
                } else clearTyping()
            }
        }

        // Seed from the app-wide cache (AppVM subscribes presence for all
        // contacts; a delta may have landed before this chat opened), then
        // track live. Subscription itself is owned by AppVM — not re-expressed
        // here, or the relay's full-set replace would narrow us to one peer.
        _presence.value = CoreBridge.presenceByPeer.value[peer.toHex()]
        viewModelScope.launch {
            CoreBridge.presence.filter { it.peer.contentEquals(peer) }.collect { sig ->
                _presence.value = sig.presence
            }
        }

        // Outbound typing: refresh under the peer's TTL while keystrokes flow,
        // one idle signal when the draft empties (send() clears input → same path).
        var lastSentAt = 0L
        viewModelScope.launch {
            input.collect { text ->
                if (text.isEmpty()) {
                    if (lastSentAt != 0L) {
                        lastSentAt = 0L
                        runCatching { CoreBridge.setActivity(peer, 0) }
                    }
                } else {
                    val now = SystemClock.uptimeMillis()
                    if (now - lastSentAt >= TYPING_RESEND_MS) {
                        lastSentAt = now
                        runCatching { CoreBridge.setActivity(peer, Activity.Typing.bit) }
                    }
                }
            }
        }
    }

    private fun clearTyping() {
        typingExpiry?.cancel()
        _typing.value = false
    }

    @Volatile
    private var limit = INITIAL_LIMIT


    /** A load returned fewer rows than asked → all history is loaded. */
    @Volatile
    private var exhausted = false
    private var loadingOlder = false

    private suspend fun load(): List<UiMessage> {
        val want = limit
        val rows = CoreBridge.messages(peer, want)                   // oldest-first
        if (rows.size < want) exhausted = true
        val byMsg = CoreBridge.reactions(peer).groupBy { it.dispatchId.toHex() }
        val media = CoreBridge.getMedia(peer).associateBy { it.dispatchId.toHex() }
        // Quote resolution: replies name a dispatch_id; snippet comes from the
        // loaded window (null text → "unavailable" shell, e.g. outside window).
        val byDid = rows.asSequence().mapNotNull { r -> r.dispatchId?.let { it.toHex() to r } }.toMap()
        // reversed → newest at index 0 → drawn at the bottom under reverseLayout;
        // AVIF decode happens in toUi, so map off the main thread.
        return withContext(Dispatchers.Default) { rows.asReversed().map { it.toUi(byMsg, byDid, media) } }
    }

    /**
     * Near-top pagination: grow the window and re-read. An accumulating beforeId
     * cursor would fight the reactive re-read (observeQuery reloads the whole
     * window on every commit); a bigger limit composes with it.
     * ponytail: grow-limit re-reads the full window per page — beforeId keyset
     * paging if that re-read ever gets too heavy.
     */
    fun loadOlder() {
        if (loadingOlder || exhausted) return
        loadingOlder = true
        limit += PAGE
        viewModelScope.launch {
            try {
                _messages.value = load()
            } finally {
                loadingOlder = false
            }
        }
    }

    fun send() {
        val text = input.value.trim()
        if (text.isEmpty()) return
        val action = composerAction.value
        input.value = ""
        composerAction.value = null
        when (action) {
            is ComposerAction.Edit -> action.msg.dispatchIdHex?.let { edit(it, text) }
            is ComposerAction.Reply -> fire {
                CoreBridge.sendMessage(peer, text, action.msg.dispatchIdHex?.fromHex())
            }
            null -> fire { CoreBridge.sendMessage(peer, text) }
        }
    }

    fun beginReply(msg: UiMessage) {
        composerAction.value = ComposerAction.Reply(msg)
    }

    fun beginEdit(msg: UiMessage) {
        composerAction.value = ComposerAction.Edit(msg)
        input.value = (msg.content as? MessageContent.Text)?.text.orEmpty()
    }

    fun cancelComposerAction() {
        if (composerAction.value is ComposerAction.Edit) input.value = ""
        composerAction.value = null
    }

    /** Tap on a quick-reaction or an existing chip: mine → remove, else add. */
    fun toggleReaction(msg: UiMessage, emoji: String) {
        val id = msg.dispatchIdHex ?: return
        val mine = msg.reactions.any { it.emoji == emoji && it.mine }
        react(id, emoji, add = !mine)
    }

    fun edit(dispatchIdHex: String, text: String) =
        fire { CoreBridge.editMessage(peer, dispatchIdHex.fromHex(), text) }

    fun delete(dispatchIdHex: String, forEveryone: Boolean) =
        fire { CoreBridge.deleteMessage(peer, dispatchIdHex.fromHex(), forEveryone) }

    fun react(dispatchIdHex: String, emoji: String, add: Boolean) =
        fire { CoreBridge.react(peer, dispatchIdHex.fromHex(), emoji, add) }

    /**
     * Picked media → inline images; videos ride the P2P attachment path (raw for
     * now). A multi-pick shares one album [group_id]; the caption rides item 0.
     */
    fun attachPhotos(uris: List<Uri>) = fire {
        val caption = takeCaption()
        val gid = albumId(uris.size)
        val cr = application.contentResolver
        uris.forEachIndexed { i, uri ->
            val cap = if (i == 0) caption else ""
            // ponytail: video sent raw over P2P — transcode + poster frame land later.
            if (cr.getType(uri)?.startsWith("video/") == true) sendPickedFile(uri, cap, gid)
            else {
                val bmp = decodeDownscaled(application, uri, INLINE_MAX_EDGE) ?: return@forEachIndexed
                CoreBridge.sendImage(peer, bmp.toRgba(), bmp.width, bmp.height, cap, gid)
            }
        }
    }

    /** Picked documents → P2P attachments; a multi-pick shares one album [group_id]. */
    fun attachFiles(uris: List<Uri>) = fire {
        val caption = takeCaption()
        val gid = albumId(uris.size)
        uris.forEachIndexed { i, uri -> sendPickedFile(uri, if (i == 0) caption else "", gid) }
    }

    /** Copy a picked uri into cache and offer it as a P2P attachment; image mimes get a preview thumb. */
    private suspend fun sendPickedFile(uri: Uri, caption: String, groupId: ByteArray?) {
        val picked = resolvePickedFile(application, uri) ?: return
        val thumb = if (picked.mime.startsWith("image/")) decodeDownscaled(application, uri, THUMB_MAX_EDGE) else null
        CoreBridge.sendAttachment(
            peer, picked.path, picked.name, picked.mime,
            thumb?.toRgba(), thumb?.width ?: 0, thumb?.height ?: 0, caption, groupId,
        )
    }

    /** One random 16-byte album id for a multi-pick; a lone item isn't an album. */
    private fun albumId(count: Int): ByteArray? = if (count > 1) Random.nextBytes(16) else null

    /** Snapshot the draft as a caption and clear it (mirrors [send]). */
    private fun takeCaption(): String = input.value.trim().also { input.value = "" }

    fun download(fileIdHex: String) = fire { CoreBridge.downloadAttachment(fileIdHex.fromHex()) }

    private fun fire(block: suspend () -> Unit) = viewModelScope.launch { runCatching { block() } }

    private companion object {
        const val TYPING_TTL_MS = 6_000L

        /** Outbound refresh cadence; must stay under the peer's [TYPING_TTL_MS]. */
        const val TYPING_RESEND_MS = 4_000L

        /** Cap the inline photo's longest edge: AVIF-compresses under libcore's 256KB budget
         *  (bigger silently drops — sendImage bails when it can't hit the budget). */
        const val INLINE_MAX_EDGE = 1600

        /** Attachment preview thumb; libcore blurs it, so tiny is plenty. */
        const val THUMB_MAX_EDGE = 256

        /** First load window: a screenful + buffer. loadOlder() pages the rest on scroll. */
        const val INITIAL_LIMIT = 40

        /** Near-top page-in growth per [loadOlder]. */
        const val PAGE = 100
    }
}

/** What the next [ChatVM.send] means: a staged reply or an in-place edit. */
sealed interface ComposerAction {
    val msg: UiMessage

    data class Reply(override val msg: UiMessage) : ComposerAction
    data class Edit(override val msg: UiMessage) : ComposerAction
}

private fun MessageRecord.toUi(
    reactionsByMsg: Map<String, List<ReactionRecord>>,
    byDid: Map<String, MessageRecord>,
    mediaByDid: Map<String, MediaRecord>,
): UiMessage {
    val didHex = dispatchId?.toHex()
    val reactions = didHex?.let { reactionsByMsg[it] }
        ?.groupBy { it.emoji }
        ?.map { (emoji, rs) -> ReactionGroup(emoji, rs.size, rs.any { it.mine }) }
        ?: emptyList()
    val quote = replyTo?.toHex()?.let { rtHex ->
        val quoted = byDid[rtHex]
        Quote(
            dispatchIdHex = rtHex,
            text = quoted?.takeIf { !it.deleted }?.content,
            outgoing = quoted?.outgoing ?: false,
        )
    }
    val payload = didHex?.let { h -> mediaByDid[h]?.toContent(h, content) } ?: MessageContent.Text(content)
    return UiMessage(
        key = didHex ?: id,
        localId = id,
        dispatchIdHex = didHex,
        content = payload,
        outgoing = outgoing,
        status = SendStatus.from(status.toInt()),
        edited = edited,
        deleted = deleted,
        timestampMs = timestamp.toLong() * 1000,
        reactions = reactions,
        quote = quote,
    )
}

/** kind: 1 = inline Image (blob), else P2P Attachment (thumb + transfer progress). */
private fun MediaRecord.toContent(dispatchIdHex: String, caption: String): MessageContent =
    if (kind.toInt() == 1) MessageContent.Image(
        caption = caption,
        bitmap = blob?.let { decodeAvifCached(dispatchIdHex, it) },
        width = width.toInt(),
        height = height.toInt(),
    ) else MessageContent.Attachment(
        caption = caption,
        name = name,
        size = size.toLong(),
        mime = mime,
        thumb = thumb?.let { decodeAvifCached(dispatchIdHex, it) },
        fileIdHex = fileId?.toHex().orEmpty(),
        transferState = transferState.toInt(),
        transferHave = transferHave.toInt(),
        transferTotal = transferTotal.toInt(),
        localPath = localPath,
    )
