package com.promtuz.chat.ui.components

import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.SizeTransform
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.animation.core.snap
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.isImeVisible
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import com.promtuz.chat.ui.text.EmojiTextField
import com.promtuz.chat.ui.text.EmojiFieldController
import com.promtuz.chat.ui.text.EmojiText
import com.promtuz.chat.ui.text.clock
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import android.Manifest
import android.content.pm.PackageManager
import android.widget.Toast
import androidx.compose.ui.platform.LocalContext
import androidx.core.content.ContextCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import com.promtuz.chat.ui.stage.ChatMotion
import kotlin.math.roundToInt
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.STAGED_ATTACHMENT
import com.promtuz.chat.domain.model.STAGED_IMAGE
import com.promtuz.chat.domain.model.acceptsStaged
import com.promtuz.chat.domain.model.previewLine
import com.promtuz.chat.navigation.LocalNavForeground
import com.promtuz.chat.presentation.viewmodel.ChatVM
import com.promtuz.chat.presentation.viewmodel.ComposerAction
import com.promtuz.chat.presentation.viewmodel.StickersVM
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.appearance.chatBarHaze
import com.promtuz.chat.ui.media.MediaViewer
import com.promtuz.chat.ui.util.freezeOnExit
import com.promtuz.chat.utils.media.StickerImages
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeEffect
import org.koin.androidx.compose.koinViewModel

private val BarMarginH = 10.dp
private val BarMarginV = 8.dp
private val BarPad = 6.dp
private val BarRadius = 26.dp

private val SlotGap = 8.dp

enum class ComposerPanelKind { Attach, Stickers }

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun ChatBottomBar(
    viewModel: ChatVM, haze: HazeState, metrics: ComposerMetrics,
    onJumpTo: (String) -> Unit = {},
    onManageStickers: () -> Unit = {},
    onCreateStickerPack: () -> Unit = {},
    onRequestGone: () -> Unit = {},
) {
    val input by viewModel.input.collectAsState()
    val action by viewModel.composerAction.collectAsState()
    val busy by viewModel.composerBusy.collectAsState()
    val error by viewModel.composerError.collectAsState()
    val feedbackContext = LocalContext.current
    LaunchedEffect(error) { error?.let { Toast.makeText(feedbackContext, it, Toast.LENGTH_LONG).show() } }

    var openPanel by remember { mutableStateOf<ComposerPanelKind?>(null) }
    // Retain the closing panel's content until its exit animation finishes.
    var shownPanel by remember { mutableStateOf(ComposerPanelKind.Attach) }
    val attachOpen = openPanel == ComposerPanelKind.Attach
    val stickersOpen = openPanel == ComposerPanelKind.Stickers
    // Hold the panel until the keyboard covers it when restoring input.
    var closingToKeyboard by remember { mutableStateOf(false) }
    val field = remember { EmojiFieldController() }
    val inputActive = LocalNavForeground.current && MediaViewer.session == null
    DisposableEffect(field, inputActive) {
        onDispose {
            // The chat stays composed behind nav animations and the media viewer. Compose's
            // keyboard controller can't hide an AndroidView editor's IME, so clear focus here.
            if (inputActive) field.clearFocus()
        }
    }
    val imeVisible = WindowInsets.isImeVisible
    var restoreKeyboard by remember { mutableStateOf(false) }
    val open = { panel: ComposerPanelKind ->
        if (openPanel == null) restoreKeyboard = imeVisible
        closingToKeyboard = false
        shownPanel = panel
        openPanel = panel
    }
    // Preserve the original keyboard state even when switching between panels.
    val closeRestoring = {
        closingToKeyboard = restoreKeyboard
        openPanel = null
        if (restoreKeyboard) {
            field.showKeyboard()
        }
    }
    val toggle = { panel: ComposerPanelKind -> if (openPanel == panel) closeRestoring() else open(panel) }
    val closeFlat = { closingToKeyboard = false; openPanel = null }
    DisposableEffect(stickersOpen) {
        viewModel.setChoosingSticker(stickersOpen)
        onDispose { viewModel.setChoosingSticker(false) }
    }
    LaunchedEffect(action) {
        if (action is ComposerAction.Edit && stickersOpen) closeFlat()
    }

    val photoPicker = rememberLauncherForActivityResult(ActivityResultContracts.PickMultipleVisualMedia()) { uris ->
        if (uris.isNotEmpty()) { viewModel.attachPhotos(uris); closeFlat() }
    }
    val filePicker = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        if (uris.isNotEmpty()) { viewModel.attachFiles(uris); closeFlat() }
    }

    BackHandler(openPanel != null) { closeRestoring() }

    val context = LocalContext.current
    val recording by viewModel.recording.collectAsState()
    val beginRecording = {
        if (!viewModel.startRecording()) {
            Toast.makeText(context, "Microphone is busy", Toast.LENGTH_SHORT).show()
        } else { closeFlat(); field.clearFocus() }
    }
    val micPermission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) beginRecording()
        else Toast.makeText(context, "Voice messages need the microphone", Toast.LENGTH_SHORT).show()
    }
    val onMic = {
        if (ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) ==
            PackageManager.PERMISSION_GRANTED
        ) beginRecording() else micPermission.launch(Manifest.permission.RECORD_AUDIO)
    }
    BackHandler(action != null && openPanel == null && recording == null) { viewModel.cancelComposerAction() }
    BackHandler(recording != null) { viewModel.cancelRecording() }
    // Android mutes a backgrounded mic, so a note left running records silence and the cap sends it.
    LifecycleEventEffect(Lifecycle.Event.ON_STOP) { viewModel.cancelRecording() }

    // No imePadding or navigationBarsPadding: ComposerPanel reserves the keyboard and nav space itself.
    val minimumPill = with(LocalDensity.current) { (38.dp + (BarPad + BarMarginV) * 2).roundToPx() }
    val actionProgress = remember { Animatable(0f) }
    val stripProgress = remember { Animatable(0f) }

    Layout(modifier = Modifier.fillMaxWidth(), content = {
        // Block content is captured (not read live) so the close animation has
        // something to draw after the action nulls.
        var lastAction by remember { mutableStateOf(action) }
        if (action != null) lastAction = action

        LaunchedEffect(action != null) {
            actionProgress.animateTo(if (action != null) 1f else 0f, ChatMotion.spec())
        }

        val staged by viewModel.staged.collectAsState()
        var lastStaged by remember { mutableStateOf(staged) }
        if (staged.isNotEmpty()) lastStaged = staged

        LaunchedEffect(staged.isEmpty()) {
            stripProgress.animateTo(if (staged.isEmpty()) 0f else 1f, ChatMotion.spec())
        }

        Box(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = BarMarginH, vertical = BarMarginV)
                .clip(RoundedCornerShape(BarRadius))
                // Freeze must sit on the same chain as the blur it bakes (screen-space
                // Haze shatters under the exiting nav card's scale).
                .freezeOnExit()
                .hazeEffect(haze, chatBarHaze())
                .padding(BarPad),
        ) {
            val request by viewModel.request.collectAsState()
            val closed by viewModel.closed.collectAsState()
            // Bottom-aligned so the composer appears where it settles as the pill's top edge glides.
            AnimatedContent(
                targetState = when {
                    request -> BarMode.Request
                    closed != null -> BarMode.Closed(closed!!)
                    else -> BarMode.Compose
                },
                transitionSpec = {
                    (fadeIn(tween(150, delayMillis = 70, easing = ChatMotion.Easing)) togetherWith fadeOut(tween(70)))
                        .using(SizeTransform(clip = true, sizeAnimationSpec = { _, _ -> ChatMotion.spec() }))
                },
                contentAlignment = Alignment.BottomStart,
                label = "requestOrComposer",
            ) { mode ->
                if (mode is BarMode.Request) RequestRow(viewModel, onRequestGone)
                else if (mode is BarMode.Closed) ClosedRow(mode.reason)
                else ComposerSlots(metrics) {
                    Reveal(actionProgress) {
                        lastAction?.let {
                            ComposerActionBlock(
                                it,
                                onCancel = { if (!busy && action != null) viewModel.cancelComposerAction() },
                                onAddMedia = { if (!busy && action != null) open(ComposerPanelKind.Attach) },
                                onJumpTo = { if (action != null) onJumpTo(it) },
                            )
                        }
                    }
                    Reveal(stripProgress) {
                        StagedStrip(lastStaged, viewModel::unstage)
                    }
                    // The recorder replaces the input row so the pill keeps one height.
                    AnimatedContent(
                        targetState = recording != null,
                        transitionSpec = { fadeIn(ChatMotion.spec()).togetherWith(fadeOut(ChatMotion.spec())) },
                        label = "composerOrRecorder",
                    ) { isRecording ->
                        if (isRecording) RecordingRow(
                            viewModel,
                            onCancel = viewModel::cancelRecording,
                            onSend = viewModel::finishRecording,
                        ) else ComposerRow(
                            viewModel, input, action,
                            attachOpen = attachOpen,
                            stickersOpen = stickersOpen,
                            metrics = metrics,
                            field = field,
                            onToggleAttach = { toggle(ComposerPanelKind.Attach) },
                            onToggleStickers = { toggle(ComposerPanelKind.Stickers) },
                            onFieldFocused = {
                                if (openPanel != null) { closingToKeyboard = true; openPanel = null } // keyboard taking over
                            },
                            onMic = onMic,
                        )
                    }
                }
            }
        }
        // Editing narrows the picker to what the target's body may become under libcore's revision rules.
        val editing = (action as? ComposerAction.Edit)?.msg?.content
        ComposerPanel(
            open = openPanel != null,
            closingToKeyboard = closingToKeyboard,
            haze = haze,
            onHideKeyboard = { field.hideKeyboard() },
        ) {
            when (shownPanel) {
                ComposerPanelKind.Attach -> AttachPanelBody(
                    allowPhotos = editing?.acceptsStaged(STAGED_IMAGE) ?: true,
                    allowFiles = editing?.acceptsStaged(STAGED_ATTACHMENT) ?: true,
                    onPickPhotos = {
                        photoPicker.launch(PickVisualMediaRequest(if (action is ComposerAction.Edit) ActivityResultContracts.PickVisualMedia.ImageOnly else ActivityResultContracts.PickVisualMedia.ImageAndVideo))
                    },
                    onPickFiles = { filePicker.launch(arrayOf("*/*")) },
                    onSendPhotos = { uris -> viewModel.attachPhotos(uris); closeFlat() },
                    onOpenCamera = {
                        com.promtuz.chat.ui.camera.CameraLauncher.open { file, video ->
                            viewModel.attachCaptured(file, video)
                            closeFlat()
                        }
                    },
                )
                ComposerPanelKind.Stickers -> {
                    val stickers = koinViewModel<StickersVM>()
                    val packs by stickers.packs.collectAsState()
                    val recents by stickers.recents.collectAsState()
                    LaunchedEffect(Unit) { stickers.refresh() }
                    StickerPanelBody(
                        packs = packs,
                        recents = recents,
                        onPick = viewModel::sendSticker,
                        onCreate = { closeFlat(); onCreateStickerPack() },
                        onManage = { closeFlat(); onManageStickers() },
                    )
                }
            }
        }
    }) { children, constraints ->
        // Reserve the keyboard/panel first, but always leave a usable input row.
        // Read back allocated heights, never the panel's requested inset.
        val available = constraints.maxHeight
        val region = children[1].measure(constraints.copy(minHeight = 0,
            maxHeight = (available - minimumPill).coerceAtLeast(0)))
        val pill = children[0].measure(constraints.copy(minHeight = 0,
            maxHeight = (available - region.height).coerceAtLeast(0)))
        metrics.composerPx = pill.height
        metrics.regionPx = region.height
        layout(constraints.maxWidth, pill.height + region.height) {
            pill.placeRelative(0, 0)
            region.placeRelative(0, pill.height)
        }
    }
}

private sealed interface BarMode {
    data object Compose : BarMode
    data object Request : BarMode
    data class Closed(val reason: String) : BarMode
}

@Composable
private fun ClosedRow(reason: String) {
    Text(reason, Modifier.fillMaxWidth().padding(vertical = 12.dp), textAlign = TextAlign.Center,
        style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
}

@Composable
private fun RequestRow(viewModel: ChatVM, onGone: () -> Unit) {
    val name by viewModel.title.collectAsState()
    var confirming by remember { mutableStateOf<RequestDecision?>(null) }
    val colors = MaterialTheme.colorScheme
    Column(Modifier.fillMaxWidth().padding(start = 10.dp, end = 4.dp, top = 6.dp)) {
        Text("They won’t know you’ve read this until you accept.",
            style = MaterialTheme.typography.bodyMedium, color = colors.onSurfaceVariant)
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = { confirming = RequestDecision.Block }) { Text("Block", color = colors.error) }
            TextButton(onClick = { confirming = RequestDecision.Delete }) { Text("Delete") }
            Spacer(Modifier.weight(1f))
            Button(onClick = { viewModel.acceptRequest() }) { Text("Accept") }
        }
    }
    confirming?.let { decision ->
        RequestConfirmDialog(decision, name,
            onConfirm = { confirming = null; viewModel.dismissRequest(decision == RequestDecision.Block, onGone) },
            onDismiss = { confirming = null })
    }
}

/** Input has priority; accessory slots share only the remaining vertical budget. */
@Composable
private fun ComposerSlots(metrics: ComposerMetrics, content: @Composable () -> Unit) {
    Layout(content = content) { children, constraints ->
        val loose = constraints.copy(minHeight = 0)
        val minimumRow = 38.dp.roundToPx().coerceAtMost(constraints.maxHeight)
        val action = children[0].measure(loose.copy(maxHeight = (constraints.maxHeight - minimumRow).coerceAtLeast(0)))
        val strip = children[1].measure(loose.copy(maxHeight = (constraints.maxHeight - minimumRow - action.height).coerceAtLeast(0)))
        val row = children[2].measure(loose.copy(maxHeight = (constraints.maxHeight - action.height - strip.height).coerceAtLeast(0)))
        metrics.accessoryPx = action.height + strip.height
        layout(constraints.maxWidth, row.height + action.height + strip.height) {
            action.placeRelative(0, 0)
            strip.placeRelative(0, action.height)
            row.placeRelative(0, action.height + strip.height)
        }
    }
}

/** Shows [progress] of its content's height from the top, fading with the same value. */
@Composable
private fun Reveal(
    progress: Animatable<Float, *>, content: @Composable () -> Unit,
) {
    Layout(
        content = content,
        modifier = Modifier
            .clipToBounds()
            .graphicsLayer { alpha = progress.value },
    ) { measurables, constraints ->
        val p = measurables.firstOrNull()?.measure(constraints)
        if (p == null) layout(0, 0) {}
        else {
            val h = (p.height * progress.value).roundToInt().coerceIn(0, p.height)
            layout(p.width, h) { p.placeRelative(0, 0) }
        }
    }
}

/** Always one line, so a swap never resizes the bar and the snippet rolls in place. */
@Composable
private fun ComposerActionBlock(
    action: ComposerAction, onCancel: () -> Unit, onAddMedia: () -> Unit,
    onJumpTo: (String) -> Unit,
) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current
    val content = action.msg.content
    val editing = action is ComposerAction.Edit
    // An album is several messages and a revision targets one, so it gets no media line.
    val offersMedia = editing && !action.msg.deleted && content !is MessageContent.Album
    val label = if (editing) "Editing" else "Replying to"

    val thumb = when (content) {
        is MessageContent.Image -> content.bitmap
        is MessageContent.Attachment -> content.thumb
        is MessageContent.Sticker -> StickerImages.peek(content.sticker)
        // An album's cover is its first member, the one that carries the caption.
        is MessageContent.Album -> content.items.firstOrNull()?.content?.let {
            when (it) {
                is MessageContent.Image -> it.bitmap
                is MessageContent.Attachment -> it.thumb
                else -> null
            }
        }
        else -> null
    }

    val snippet = when {
        action.msg.deleted -> "Deleted message"
        editing -> when (content) {
            is MessageContent.Image -> "Tap to replace photo"
            is MessageContent.Attachment -> "Tap to replace file"
            is MessageContent.Album -> content.previewLine()
            else -> "Tap to add media"
        }
        else -> content.previewLine()
    }

    Row(
        Modifier
            .fillMaxWidth()
            .padding(start = 10.dp, top = 4.dp, bottom = 10.dp, end = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (content !is MessageContent.Text && !action.msg.deleted) {
            Box(
                Modifier
                    .padding(end = 8.dp)
                    .size(36.dp)
                    .clip(RoundedCornerShape(8.dp))
                    .background(colors.surfaceVariant),
                contentAlignment = Alignment.Center,
            ) {
                if (thumb != null) Image(
                    thumb, null, Modifier.fillMaxSize(),
                    contentScale = if (content is MessageContent.Sticker) ContentScale.Fit else ContentScale.Crop,
                )
                else DrawableIcon(
                    when (content) {
                        is MessageContent.Voice -> R.drawable.i_mic
                        is MessageContent.Sticker -> R.drawable.oi_sticker
                        else -> R.drawable.oi_paperclip
                    },
                    Modifier.size(16.dp),
                    tint = colors.onSurfaceVariant,
                )
            }
        }
        val onLineTap: (() -> Unit)? = when {
            offersMedia -> onAddMedia
            !editing -> action.msg.dispatchIdHex?.let { did -> { onJumpTo(did) } }
            else -> null
        }
        Column(
            Modifier
                .weight(1f)
                .then(onLineTap?.let { Modifier.clickable(onClick = it) } ?: Modifier),
        ) {
            Text(label, style = MaterialTheme.typography.labelMedium, color = chat.accent)
            AnimatedContent(
                targetState = snippet,
                transitionSpec = {
                    (slideInVertically(ChatMotion.spec()) { it } + fadeIn(ChatMotion.spec()))
                        .togetherWith(
                            slideOutVertically(ChatMotion.spec()) { -it } + fadeOut(ChatMotion.spec())
                        )
                },
                label = "actionSnippet",
            ) { s ->
                EmojiText(
                    s,
                    style = MaterialTheme.typography.bodyMedium,
                    color = if (offersMedia) chat.accent else colors.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        // Not an IconButton: its 48dp minimum would set the block's height.
        Box(
            Modifier.size(38.dp).clip(CircleShape).clickable(onClick = onCancel)
                .semantics { contentDescription = "Cancel action" },
            contentAlignment = Alignment.Center,
        ) {
            MorphIcon(MorphGlyph.Close, null, Modifier.size(18.dp), tint = colors.onSurfaceVariant)
        }
    }
}

@OptIn(androidx.compose.foundation.ExperimentalFoundationApi::class)
@Composable
private fun ComposerRow(
    viewModel: ChatVM,
    input: String,
    action: ComposerAction?,
    attachOpen: Boolean,
    stickersOpen: Boolean,
    metrics: ComposerMetrics,
    field: EmojiFieldController,
    onToggleAttach: () -> Unit,
    onToggleStickers: () -> Unit,
    onFieldFocused: () -> Unit,
    onMic: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current

    // Send is held while anything is encoding: libcore refuses a half-prepared item.
    val staged by viewModel.staged.collectAsState()
    val sending by viewModel.composerBusy.collectAsState()
    val sentRevision by viewModel.sentRevision.collectAsState()
    val textExit = rememberComposerTextExit(sentRevision, input) { metrics.sendTransition = it }
    val sendError by viewModel.composerError.collectAsState()
    LaunchedEffect(sendError) { if (sendError != null) textExit.reject() }
    val busy = sending || textExit.capturing
    val editing = action as? ComposerAction.Edit
    val canClearCaption = editing?.msg?.content is MessageContent.Image || editing?.msg?.content is MessageContent.Attachment
    val hasContent = input.isNotBlank() || staged.isNotEmpty()
    val hasDraft = (hasContent || canClearCaption) && staged.all { it.ready } && !busy

    Row(
        modifier.fillMaxWidth(),
        verticalAlignment = Alignment.Bottom,
    ) {
        // Paperclip while drafting, stickers otherwise. A sticker can't replace an
        // edit's body, so the slot sits out of an edit.
        Box(
            Modifier.padding(end = SlotGap).size(38.dp).clip(CircleShape)
                .clickable(enabled = !busy && (hasContent || action !is ComposerAction.Edit)) {
                    if (hasContent) onToggleAttach() else onToggleStickers()
                }
                .semantics { contentDescription = if (hasContent) "Attach media" else "Stickers" },
            contentAlignment = Alignment.Center,
        ) {
            AnimatedContent(
                targetState = hasContent,
                transitionSpec = {
                    (fadeIn(ChatMotion.spec()) + scaleIn(ChatMotion.spec(), 0.7f))
                        .togetherWith(fadeOut(ChatMotion.spec()) + scaleOut(ChatMotion.spec(), 0.7f))
                },
                label = "leadingSlot",
            ) { drafting ->
                DrawableIcon(
                    if (drafting) R.drawable.oi_paperclip else R.drawable.oi_sticker,
                    Modifier.size(if (drafting) 20.dp else 22.dp),
                    tint = if (if (drafting) attachOpen else stickersOpen) chat.accent else colors.onSurfaceVariant,
                )
            }
        }
        EmojiTextField(
            value = input,
            onValueChange = { if (!busy) viewModel.input.value = it },
            acceptChanges = !busy,
            textStyle = MaterialTheme.typography.bodyLarge.copy(color = colors.onSurface),
            cursorColor = chat.accent,
            controller = field,
            onFieldFocused = onFieldFocused,
            onReceiveImages = viewModel::attachPhotos,
            maxLines = 6,
            modifier = Modifier.weight(1f)
                .then(textExit.modifier)
                .animateContentSize(if (textExit.fading) snap() else ChatMotion.spec(), alignment = Alignment.BottomStart),
            // Floored at the button size and centred, so a single line sits level with the icons.
            decorationBox = { inner ->
                Box(
                    Modifier.heightIn(min = 38.dp).padding(vertical = 7.dp),
                    contentAlignment = Alignment.CenterStart,
                ) {
                    if (input.isEmpty()) Text(
                        if (staged.isEmpty()) "Message" else "Caption",
                        style = MaterialTheme.typography.bodyLarge,
                        color = colors.onSurfaceVariant,
                    )
                    inner()
                }
            },
        )


        // Keep the same text width while the attachment affordance changes sides.
        Box(Modifier.padding(start = SlotGap).size(38.dp)
            .graphicsLayer { alpha = if (hasContent) 0f else 1f }
            .clip(CircleShape).clickable(enabled = !hasContent && !busy) { onToggleAttach() }
            .semantics { contentDescription = "Attach media" },
            contentAlignment = Alignment.Center) {
            DrawableIcon(R.drawable.oi_paperclip, Modifier.size(20.dp),
                tint = if (attachOpen) chat.accent else colors.onSurfaceVariant)
        }

        // Always occupied at a fixed size so the pill's height never jumps. Solid accent,
        // no haze: a blurred layer under the circle renders as a square.
        Box(
            Modifier
                .padding(start = SlotGap)
                .size(38.dp)
                .clip(CircleShape)
                .background(if (hasDraft) chat.accent else Color.Transparent)
                .semantics { contentDescription = if (hasContent || action is ComposerAction.Edit) "Send message" else "Record voice message" }
                .clickable(enabled = !busy && (hasDraft || (!hasContent && action !is ComposerAction.Edit))) {
                    if (hasDraft) {
                        if (input.isNotEmpty() && action !is ComposerAction.Edit) textExit.submit(viewModel::send)
                        else viewModel.send()
                    } else onMic()
                },
            contentAlignment = Alignment.Center,
        ) {
            // hasContent, not hasDraft: a mic would promise a recording the held slot can't start.
            if (busy) CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp, color = chat.accent)
            else AnimatedContent(
                targetState = when {
                    action is ComposerAction.Edit -> R.drawable.i_edit_check
                    hasContent -> R.drawable.i_send
                    else -> R.drawable.i_mic
                },
                transitionSpec = {
                    (scaleIn(tween(140), 0.6f) + fadeIn(tween(140)))
                        .togetherWith(scaleOut(tween(140), 0.6f) + fadeOut(tween(140)))
                },
                label = "composerAction",
            ) { icon ->
                DrawableIcon(
                    icon,
                    Modifier.size(18.dp),
                    tint = if (hasDraft) colors.onPrimary else colors.onSurfaceVariant,
                )
            }
        }
    }
}

@Composable
private fun RecordingRow(viewModel: ChatVM, onCancel: () -> Unit, onSend: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current
    val recording by viewModel.recording.collectAsState()
    val level by animateFloatAsState(recording?.level ?: 0f, tween(100), label = "micLevel")
    Row(
        Modifier.fillMaxWidth().heightIn(min = 38.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(38.dp), contentAlignment = Alignment.Center) {
            Box(
                Modifier
                    .size((10 + 14 * level).dp)
                    .clip(CircleShape)
                    .background(colors.error.copy(alpha = 0.25f + 0.75f * (1f - level))),
            )
        }
        Text(
            clock(recording?.elapsedMs ?: 0L),
            style = MaterialTheme.typography.bodyLarge,
            color = colors.onSurface,
            modifier = Modifier.padding(start = 4.dp),
        )
        Text(
            "Recording…",
            style = MaterialTheme.typography.bodyMedium,
            color = colors.onSurfaceVariant,
            modifier = Modifier.padding(start = 12.dp).weight(1f),
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        Box(
            Modifier.size(38.dp).clip(CircleShape).clickable(onClick = onCancel),
            contentAlignment = Alignment.Center,
        ) {
            MorphIcon(MorphGlyph.Close, null, Modifier.size(18.dp), tint = colors.onSurfaceVariant)
        }
        Box(
            Modifier
                .padding(start = SlotGap)
                .size(38.dp)
                .clip(CircleShape)
                .background(chat.accent)
                .clickable(onClick = onSend),
            contentAlignment = Alignment.Center,
        ) {
            DrawableIcon(R.drawable.i_send, Modifier.size(18.dp), tint = colors.onPrimary)
        }
    }
}
