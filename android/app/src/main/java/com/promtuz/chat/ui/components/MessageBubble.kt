package com.promtuz.chat.ui.components

import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.Animatable
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.awaitLongPressOrCancellation
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.blur
import androidx.compose.ui.draw.BlurredEdgeTreatment
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.boundsInRoot
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.TextLayoutResult
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.dp
import kotlin.math.ceil
import com.promtuz.chat.domain.model.MessageContent
import androidx.compose.ui.text.font.FontWeight
import com.promtuz.chat.domain.model.Quote
import kotlin.math.absoluteValue
import com.promtuz.chat.domain.model.ReactionGroup
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.stage.ChatMotion
import com.promtuz.chat.ui.theme.PromtuzTheme
import android.content.res.Configuration
import android.os.Build

/**
 * The bubble's inner inset. The vertical half doubles as the meta's drop budget.
 * Media bubbles waive it — the picture goes to the bubble's own edge and the
 * blocks below it re-apply the inset themselves.
 */
internal val BubblePadH = 11.dp
internal val BubblePadV = 6.dp

/**
 * How far the meta sits below the text's last line when it rides beside it, so the
 * timestamp settles under the baseline rather than reading level with the text.
 *
 * This is spent out of [BubblePadV], not added to the bubble's height — the meta
 * hangs into the existing padding, so the bubble never grows for it. Values past
 * that budget are clamped: the [clip] to the bubble shape would crop the glyphs.
 */
private val MetaBaselineDrop = 3.dp

/** How hard the patch under an on-picture meta is blurred. Large next to the
 *  glyphs it backs — the softness IS the effect, not an edge to hide. */
private val MetaHaloBlur = 22.dp

/**
 * How far the patch runs past the text. Grown mostly down and toward the end —
 * the corner the meta occupies — so it spills past the bubble and the bubble's
 * own clip trims it to the corner radius. That's what keeps it reading as the
 * picture darkening into its corner rather than a blob floating on top of it.
 */
private const val MetaHaloSpreadX = 2.6f
private const val MetaHaloSpreadY = 3.4f

/**
 * A message bubble as an ordered stack of content blocks (text today; media /
 * reply become sibling blocks with the polymorphic content). Shape/colors/width
 * come from [LocalChatAppearance]. The trailing meta — a sent-time, or a spinner
 * for a not-yet-sent message — is pinned to the bubble's bottom-end corner; the
 * bubble widens to seat it beside the text's last line, or gives it a compact row
 * of its own when that line has no room.
 * No per-message ticks: delivery state rides the frontier markers.
 *
 * [onLongPress] (fired with the row's root bounds, for the context-menu lift),
 * [onReactionTap], [onQuoteClick] (fired with the quoted message's dispatch id)
 * and [onDoubleTap] are optional so the bubble stays a pure renderer elsewhere.
 * With [menuState] set, the long-press gesture keeps streaming into the open
 * menu — drag over an item, release to pick it (one continuous pointer stream,
 * same interaction grammar as AppDropMenu).
 */
@Composable
fun MessageBubble(
    modifier: Modifier = Modifier,
    msg: UiMessage,
    mergedTop: Boolean = false,
    mergedBottom: Boolean = false,
    onLongPress: ((Rect) -> Unit)? = null,
    menuState: MessageMenuState? = null,
    onReactionTap: ((String) -> Unit)? = null,
    onQuoteClick: ((String) -> Unit)? = null,
    onDoubleTap: (() -> Unit)? = null,
    onDownload: ((String) -> Unit)? = null,
    onOpen: ((String) -> Unit)? = null,
    peerName: String = "",
) {
    val appearance = LocalChatAppearance.current
    val chat = LocalChatColors.current
    val outgoing = msg.outgoing
    // Name the author only in a group, only on an incoming message, and only
    // at the head of a run — the rest of the run is visibly the same person.
    val showSender = msg.senderName != null && !outgoing && !mergedTop
    val shape = rememberBubbleShape(outgoing, mergedTop, mergedBottom, appearance.bubble)
    val bubbleColor = if (outgoing) chat.outgoingBubble else chat.incomingBubble
    val textColor = if (outgoing) chat.onOutgoingBubble else chat.onIncomingBubble
    val haptic = LocalHapticFeedback.current
    // Plain refs, not snapshot state: positions change every frame during placement
    // animations and are only ever read inside gesture handlers (and, for the text
    // layout, inside the measure pass that just wrote it).
    val coords = remember { CoordsHolder() }
    // pointerInput keys on `menuState`, which is a stable holder, so the gesture
    // coroutine is started once and closes over whatever `onLongPress` existed
    // then. A message that later changes shape — a text edited into a picture —
    // would still open the menu on the body it had at first composition.
    val longPress by rememberUpdatedState(onLongPress)
    val isTextBlock = msg.deleted || msg.content is MessageContent.Text

    // A picture runs to the bubble's own edge — one outline instead of a frame
    // around a frame — so the bubble waives its inset and the padded blocks
    // (quote, caption, reactions) each put it back for themselves. An attachment
    // card is not a picture: it keeps the inset like text.
    val bleeds = !msg.deleted &&
        (msg.content is MessageContent.Image || msg.content is MessageContent.Album)
    val caption = when (val c = msg.content) {
        is MessageContent.Image -> c.caption
        is MessageContent.Album -> c.caption
        else -> ""
    }
    // With nothing below it to sit in, the time has to ride the picture itself.
    val metaOnMedia = bleeds && caption.isEmpty() && msg.reactions.isEmpty()

    // Plain Box, not BoxWithConstraints — that's a nested SubcomposeLayout per
    // bubble, real weight on every bubble birth. The width cap is applied inside
    // the bubble Layout's own measure from its incoming constraints.
    val widthFraction = appearance.layout.maxWidthFraction
    Box(
        modifier
            .fillMaxWidth()
            .onGloballyPositioned { coords.row = it }
            .padding(horizontal = 12.dp),
        contentAlignment = if (outgoing) Alignment.CenterEnd else Alignment.CenterStart,
    ) {
        Layout(
            content = {
                // Only the first bubble of a run is labelled — repeating the
                // name down a run of five messages is noise, not information.
                if (showSender) {
                    SenderLabel(
                        msg.senderName ?: "Unknown",
                        msg.senderHex,
                        modifier = if (bleeds)
                            Modifier.padding(start = BubblePadH, end = BubblePadH, top = BubblePadV)
                        else Modifier,
                    )
                }
                msg.quote?.let { q ->
                    QuoteBlock(
                        q, textColor, chat.accent, onQuoteClick?.let { cb -> { cb(q.dispatchIdHex) } },
                        modifier = if (bleeds)
                            Modifier.padding(start = BubblePadH, end = BubblePadH, top = BubblePadV)
                        else Modifier,
                    )
                }

                // One content child in a fixed slot: the bubble Layout hardcodes child
                // indices, so a deleted-or-text bubble and a media bubble must emit exactly
                // one measurable here. The meta corner is reserved inside each variant.
                val content = msg.content
                when {
                    msg.deleted || content is MessageContent.Text ->
                        BubbleText(msg, textColor, appearance.type.fontScale) { coords.text = it }
                    content is MessageContent.Image ->
                        ImageBlock(content, textColor, appearance.type.fontScale, BubbleTextLayouts.metaLabelOf(msg))
                    content is MessageContent.Album ->
                        AlbumBlock(content, textColor, appearance.type.fontScale, BubbleTextLayouts.metaLabelOf(msg))
                    content is MessageContent.Attachment ->
                        AttachmentBlock(
                            content, textColor, appearance.type.fontScale,
                            BubbleTextLayouts.metaLabelOf(msg), peerName, outgoing, onDownload, onOpen,
                        )
                }

                if (msg.reactions.isNotEmpty()) {
                    Row(
                        if (bleeds) Modifier.padding(
                            start = BubblePadH, end = BubblePadH, top = 4.dp, bottom = BubblePadV,
                        ) else Modifier.padding(top = 4.dp),
                        horizontalArrangement = Arrangement.spacedBy(4.dp),
                    ) {
                        msg.reactions.forEach { rg ->
                            ReactionChip(rg, textColor, chat.accent, onReactionTap)
                        }
                    }
                }

                MetaRow(msg, textColor, metaOnMedia)
            },
            modifier = Modifier
                // Fill FIRST, before animateContentSize (which opens with clipToBounds) and
                // the .clip below. Both clip to the node's rectangular bounds, which would
                // shear off the tail flicking past the body edge. As a plain draw modifier
                // here, background paints the whole outline (tail included) into the parent
                // Box (which never clips); .clip still bounds the child content below.
                .background(bubbleColor, shape)
                // edit/delete/reactions change the bubble's size in place — glide from the
                // tail corner on the shared clock so neighbors (stage) track frame-locked
                .animateContentSize(
                    ChatMotion.spec(),
                    alignment = if (outgoing) Alignment.BottomEnd else Alignment.BottomStart,
                )
                .clip(shape)
                .onGloballyPositioned { coords.bubble = it }
                .then(
                    if (onLongPress == null) Modifier
                    else Modifier.pointerInput(menuState) {
                        awaitEachGesture {
                            val down = awaitFirstDown(requireUnconsumed = false)
                            if (menuState?.isOpen == true) return@awaitEachGesture
                            val press =
                                awaitLongPressOrCancellation(down.id) ?: return@awaitEachGesture
                            haptic.performHapticFeedback(HapticFeedbackType.LongPress)
                            longPress?.invoke(
                                coords.row?.takeIf { it.isAttached }?.boundsInRoot() ?: Rect.Zero
                            )
                            if (menuState == null) return@awaitEachGesture

                            // Same finger now drives the open menu: drag hovers, release picks.
                            var dragged = false
                            while (true) {
                                val ev = awaitPointerEvent()
                                val ch = ev.changes.firstOrNull { it.id == press.id }
                                    ?: ev.changes.first()
                                val root = coords.bubble?.takeIf { it.isAttached }
                                    ?.localToRoot(ch.position)
                                if (!ch.pressed) {
                                    // Commits require an actual drag: a stationary hold-and-lift
                                    // only leaves the menu open, even if an item spawned under
                                    // the finger. Drag to nowhere cancels.
                                    if (dragged) when (val hit = root?.let(menuState::release)) {
                                        is MenuHit.Action -> {
                                            haptic.performHapticFeedback(HapticFeedbackType.Confirm)
                                            hit.action.onClick()
                                        }

                                        is MenuHit.Reaction -> {
                                            haptic.performHapticFeedback(HapticFeedbackType.Confirm)
                                            menuState.onReact?.invoke(hit.emoji)
                                        }

                                        null -> menuState.close()
                                    }
                                    break
                                }
                                if (!dragged &&
                                    (ch.position - down.position).getDistance() > viewConfiguration.touchSlop
                                ) dragged = true
                                if (dragged && root != null && menuState.drag(root)) {
                                    haptic.performHapticFeedback(HapticFeedbackType.SegmentTick)
                                }
                                ch.consume()
                            }
                        }
                    }
                )
                .then(
                    if (onDoubleTap == null) Modifier
                    else Modifier.pointerInput(onDoubleTap) {
                        detectTapGestures(onDoubleTap = { onDoubleTap() })
                    }
                )
                .padding(
                    horizontal = if (bleeds) 0.dp else BubblePadH,
                    vertical = if (bleeds) 0.dp else BubblePadV,
                ),
        ) { measurables, constraints ->
            // Children: [quote?] text [reactions?] meta. The quote must span the widest
            // sibling (measured last with that width as its minimum — measurables measure
            // once); the meta is pinned to the bubble's absolute bottom-end corner, and the
            // bubble grows to keep it off the content — see the contentWidth branches below.
            val hasSender = showSender
            val hasQuote = msg.quote != null
            val hasReactions = msg.reactions.isNotEmpty()
            val cap = (constraints.maxWidth * widthFraction).toInt()
            val loose = Constraints(maxWidth = cap)
            val leading = (if (hasSender) 1 else 0) + (if (hasQuote) 1 else 0)
            var idx = leading
            val text = measurables[idx].measure(loose)
            val reactions = if (hasReactions) measurables[++idx].measure(loose) else null
            val meta = measurables[idx + 1].measure(loose)

            // Where the text's own last line ends — populated by BubbleText's onTextLayout
            // during the measure above. Null for media blocks, which reserve their corner
            // internally and own their full footprint.
            val lastLine = coords.text
                ?.takeIf { it.lineCount > 0 }
                ?.let { ceil(it.getLineRight(it.lineCount - 1)).toInt() }

            val metaGap = 8.dp.roundToPx()
            // The meta rides the text's last line when that line leaves room for it; when it
            // doesn't, it drops to a row of its own — meta-height, not a full text line, so a
            // wrapped timestamp doesn't leave a tall empty gutter. Reactions, when present,
            // share their line with the meta instead and take priority.
            var metaRow = 0
            // Riding the last line, the meta would otherwise bottom-align with the text's
            // line box and read as level with it. This settles it just under the baseline,
            // spent out of the bubble's own padding so the bubble doesn't grow — see the
            // height below, which deliberately leaves metaDrop out.
            var metaDrop = 0
            val contentWidth = when {
                hasReactions -> maxOf(text.width, reactions!!.width + metaGap + meta.width)
                // Media owns its whole footprint and keeps the corner clear itself.
                !isTextBlock -> text.width
                // No layout to consult (shouldn't happen — onTextLayout runs in the measure
                // above): give the meta its own row rather than risk it landing on a glyph.
                lastLine == null -> { metaRow = meta.height; text.width }
                lastLine + metaGap + meta.width <= cap -> {
                    metaDrop = MetaBaselineDrop.coerceAtMost(BubblePadV).roundToPx()
                    maxOf(text.width, lastLine + metaGap + meta.width)
                }
                else -> { metaRow = meta.height; text.width }
            }
            // The sender label and the quote both span the widest sibling, so
            // they measure last with the settled content width as their floor.
            val sender = if (hasSender) measurables[0].measure(loose) else null
            val quote = if (hasQuote) {
                measurables[if (hasSender) 1 else 0].measure(loose.copy(minWidth = contentWidth))
            } else null

            val width = maxOf(contentWidth, maxOf(quote?.width ?: 0, sender?.width ?: 0))
            // metaDrop is absent by design: it's spent out of the padding, so it moves the
            // meta without moving the Layout. metaRow can't be — a row of its own needs
            // real space, and the padding alone can't cover a full meta height.
            val height = (sender?.height ?: 0) + (quote?.height ?: 0) + text.height + metaRow +
                (reactions?.height ?: 0)
            layout(width, height) {
                var y = 0
                sender?.let { it.placeRelative(0, y); y += it.height }
                quote?.let { it.placeRelative(0, y); y += it.height }
                text.placeRelative(0, y)
                reactions?.placeRelative(0, y + text.height)
                // A bleeding bubble has no outer padding to sit in, so the meta
                // takes the inset itself — the same one text bubbles get from the
                // padding, which is what lines the time up across every variant.
                val metaInsetX = if (bleeds) BubblePadH.roundToPx() else 0
                val metaInsetY = if (bleeds) BubblePadV.roundToPx() else 0
                meta.placeRelative(
                    width - meta.width - metaInsetX + metaDrop,
                    height - meta.height - metaInsetY + metaDrop,
                )
            }
        }
    }
}

/** The quoted-message block a reply carries: accent rail + short snippet. */
@Composable
private fun QuoteBlock(
    quote: Quote, textColor: Color, accent: Color, onClick: (() -> Unit)?,
    modifier: Modifier = Modifier,
) {
    Row(
        modifier
            .padding(top = 2.dp, bottom = 4.dp)
            .clip(RoundedCornerShape(6.dp))
            .background(textColor.copy(alpha = 0.08f))
            .then(onClick?.let { Modifier.clickable(onClick = it) } ?: Modifier)
            .height(IntrinsicSize.Min),
    ) {
        Box(Modifier
            .width(3.dp)
            .fillMaxHeight()
            .background(accent))
        Text(
            quote.text ?: "Message unavailable",
            Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
            style = MaterialTheme.typography.bodySmall,
            color = textColor.copy(alpha = if (quote.text != null) 0.8f else 0.5f),
            fontStyle = if (quote.text != null) FontStyle.Normal else FontStyle.Italic,
            maxLines = 2,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

@Composable
private fun ReactionChip(rg: ReactionGroup, textColor: Color, accent: Color, onTap: ((String) -> Unit)?) {
    Row(
        Modifier
            .clip(RoundedCornerShape(10.dp))
            .background(if (rg.mine) accent.copy(alpha = 0.35f) else textColor.copy(alpha = 0.10f))
            .then(onTap?.let { Modifier.clickable { it(rg.emoji) } } ?: Modifier)
            .padding(horizontal = 7.dp, vertical = 3.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(rg.emoji, style = MaterialTheme.typography.labelMedium)
        if (rg.count > 1) Text(
            " ${rg.count}",
            style = MaterialTheme.typography.labelSmall,
            color = textColor.copy(alpha = 0.85f),
        )
    }
}

/**
 * The message text, wrapped purely on its own content. It reserves nothing for the
 * meta: [onLayout] hands the resolved [TextLayoutResult] up to the bubble's Layout
 * (it fires during this node's measure, so the parent reads it in the same pass),
 * which then decides whether the meta rides the last line or takes its own row.
 */
@Composable
private fun BubbleText(
    msg: UiMessage,
    textColor: Color,
    fontScale: Float,
    onLayout: (TextLayoutResult) -> Unit,
) {
    val color = if (msg.deleted) textColor.copy(alpha = 0.6f) else textColor
    val text = BubbleTextLayouts.contentOf(msg)

    val base = MaterialTheme.typography.bodyLarge
    Text(
        text,
        Modifier.fadeOnChange(text),
        style = base.copy(fontSize = base.fontSize * fontScale, color = color),
        fontStyle = if (msg.deleted) FontStyle.Italic else FontStyle.Normal,
        color = color,
        onTextLayout = onLayout,
    )
}

/** Fades the node in when [value] changes after first composition. Near-free at rest. */
@Composable
private fun Modifier.fadeOnChange(value: Any?): Modifier {
    val anim = remember { Animatable(1f) }
    var last by remember { mutableStateOf(value) }
    LaunchedEffect(value) {
        if (last != value) {
            last = value
            anim.snapTo(0f)
            anim.animateTo(1f, ChatMotion.spec())
        }
    }
    return graphicsLayer { alpha = anim.value }
}

/**
 * Pending spinner / failed dot / sent time, crossfading inside the corner slot.
 *
 * [onMedia] is the uncaptioned-picture case: the meta lands on the photo, where
 * `textColor` at 55% would be unreadable over an arbitrary image, so it goes
 * white against the gradient the media block lays under it. Its position doesn't
 * change — that's the point of the gradient over a chip.
 */
@Composable
private fun MetaRow(msg: UiMessage, textColor: Color, onMedia: Boolean = false) {
    val metaStyle = MaterialTheme.typography.labelSmall
    val metaColor = if (onMedia) Color.White else textColor.copy(alpha = 0.55f)
    val edited = msg.edited && !msg.deleted
    val state = when {
        msg.outgoing && msg.status == SendStatus.Pending -> MetaState.Pending
        msg.outgoing && msg.status == SendStatus.Failed -> MetaState.Failed
        else -> MetaState.Sent
    }

    Box(contentAlignment = Alignment.Center) {
        if (onMedia) MetaHalo(Modifier.matchParentSize())
        Row(verticalAlignment = Alignment.CenterVertically) {
        if (edited) Text(
            "edited",
            style = metaStyle,
            color = metaColor,
            modifier = Modifier.padding(end = 4.dp),
        )
        Box(Modifier.fadeOnChange(state), contentAlignment = Alignment.CenterEnd) {
            when (state) {
                MetaState.Pending ->
                    CircularProgressIndicator(Modifier.size(11.dp), color = metaColor, strokeWidth = 1.5.dp)
                MetaState.Failed ->
                    Box(Modifier
                        .size(9.dp)
                        .clip(CircleShape)
                        .background(MaterialTheme.colorScheme.error))
                MetaState.Sent ->
                    Text(BubbleTextLayouts.clock(msg.timestampMs), style = metaStyle, color = metaColor)
            }
        }
        }
    }
}

/**
 * The patch of dimmed picture an on-media meta sits on.
 *
 * A dark shape blurred well past its own bounds, not a drawn ramp: the falloff
 * comes from the blur, so it reads as the photo darkening under the time rather
 * than a band laid across it, and it stays local to the glyphs instead of
 * banding the full width. `Modifier.blur` is a no-op below API 31, so there a
 * radial ramp stands in — the same shape of falloff, and never a hard edge.
 */
@Composable
private fun MetaHalo(modifier: Modifier) {
    val shaped = modifier.graphicsLayer {
        scaleX = MetaHaloSpreadX
        scaleY = MetaHaloSpreadY
        // Anchored up-and-start, so the growth runs the other way: into the
        // bottom-end corner, where the clip is waiting for it.
        transformOrigin = TransformOrigin(0.12f, 0.1f)
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        Box(
            shaped
                .blur(MetaHaloBlur, BlurredEdgeTreatment.Unbounded)
                .background(Color.Black.copy(alpha = 0.5f), CircleShape),
        )
    } else {
        Box(
            shaped.background(
                Brush.radialGradient(listOf(Color.Black.copy(alpha = 0.4f), Color.Transparent)),
            ),
        )
    }
}

private enum class MetaState { Pending, Failed, Sent }

private class CoordsHolder {
    var row: LayoutCoordinates? = null
    var bubble: LayoutCoordinates? = null

    /** Last text layout, written during the text child's measure and read right after it. */
    var text: TextLayoutResult? = null
}




// === // === // === // === // === // === // === // === // === // === // === // === // === // === // === //

/**
 * Sample [UiMessage] factory for previews — only the fields a preview usually
 * varies are parameters; the rest carry sensible constants. The bubble is a pure
 * renderer, so no callbacks/menuState are wired here.
 */
private fun previewMsg(
    text: String,
    outgoing: Boolean,
    id: String,
    status: SendStatus = SendStatus.Read,
    edited: Boolean = false,
    deleted: Boolean = false,
    reactions: List<ReactionGroup> = emptyList(),
    quote: Quote? = null,
    content: MessageContent = MessageContent.Text(text),
) = UiMessage(
    key = id,
    localId = id,
    dispatchIdHex = id,
    content = content,
    outgoing = outgoing,
    status = status,
    edited = edited,
    deleted = deleted,
    timestampMs = 1_700_000_000_000L,
    reactions = reactions,
    quote = quote,
)

/**
 * The meta's wrap boundary: messages stepping up in length until the last line
 * stops leaving room for the timestamp and it drops to a row of its own.
 */
@Preview(name = "Meta wrap", showBackground = true)
@Composable
private fun MessageBubbleMetaWrapPreview() {
    PromtuzTheme {
        Column(
            Modifier
                .background(MaterialTheme.colorScheme.background)
                .padding(vertical = 10.dp),
            verticalArrangement = Arrangement.spacedBy(3.dp),
        ) {
            listOf(
                "Hi",
                "Hey! Did the preview",
                "Hey! Did the preview land",
                "Hey! Did the preview land yet?",
                "Hey! Did the preview land yet? Checking the wrap boundary here.",
            ).forEachIndexed { i, t ->
                MessageBubble(msg = previewMsg(t, outgoing = i % 2 == 1, id = "w$i"))
            }
        }
    }
}

/** A whole conversation's worth of bubble states, stacked. */
@Composable
private fun MessageBubbleGallery() {
    Column(
        Modifier
            .background(MaterialTheme.colorScheme.background)
            .padding(vertical = 10.dp),
        verticalArrangement = Arrangement.spacedBy(3.dp),
    ) {
        MessageBubble(msg = previewMsg("Hey! Did the preview land yet?", outgoing = false, id = "1"))
        MessageBubble(
            msg = previewMsg(
                "Just wiring them up now — a full gallery of states so you can eyeball the layout.",
                outgoing = true, id = "2",
            ),
        )
        MessageBubble(
            msg = previewMsg(
                "Nice, this one is a reply.", outgoing = false, id = "3",
                quote = Quote(dispatchIdHex = "2", text = "Just wiring them up now…", outgoing = true),
            ),
        )
        MessageBubble(
            msg = previewMsg(
                "Loved it ❤️", outgoing = true, id = "4", edited = true,
                reactions = listOf(ReactionGroup("❤️", 2, true), ReactionGroup("🔥", 1, false)),
            ),
        )
        MessageBubble(msg = previewMsg("Sending…", outgoing = true, id = "5", status = SendStatus.Pending))
        MessageBubble(msg = previewMsg("Didn't go through", outgoing = true, id = "6", status = SendStatus.Failed))
        MessageBubble(msg = previewMsg("", outgoing = false, id = "7", deleted = true))
        MessageBubble(
            msg = previewMsg(
                "A longer incoming message to check how the bubble wraps across multiple " +
                    "lines and reserves the trailing meta corner without colliding with text.",
                outgoing = false, id = "8",
            ),
        )
    }
}

// === // === // === // === // === // === // === // === // === // === // === // === // === // === // === //

@Preview(name = "Light", showBackground = true)
@Preview(name = "Dark", showBackground = true, uiMode = Configuration.UI_MODE_NIGHT_YES)
@Composable
private fun MessageBubblePreview() {
    PromtuzTheme { MessageBubbleGallery() }
}

/** Merged run — top/middle/bottom of a same-sender group; checks the shape flags. */
@Preview(name = "Merged run", showBackground = true)
@Composable
private fun MessageBubbleMergedPreview() {
    PromtuzTheme {
        Column(
            Modifier
                .background(MaterialTheme.colorScheme.background)
                .padding(vertical = 10.dp),
            verticalArrangement = Arrangement.spacedBy(2.dp),
        ) {
            MessageBubble(msg = previewMsg("First in the run", outgoing = true, id = "m1"), mergedBottom = true)
            MessageBubble(msg = previewMsg("Middle one", outgoing = true, id = "m2"), mergedTop = true, mergedBottom = true)
            MessageBubble(msg = previewMsg("Last in the run", outgoing = true, id = "m3"), mergedTop = true)
        }
    }
}
/**
 * The author's name atop the first incoming bubble of a run in a group.
 * Coloured from a hash of their key, so the same person keeps the same colour
 * across the conversation without anyone assigning one.
 */
@Composable
private fun SenderLabel(name: String, key: String?, modifier: Modifier = Modifier) {
    val palette = LocalChatColors.current.senderPalette
    val color = remember(key, palette) {
        palette[((key?.hashCode() ?: 0).absoluteValue) % palette.size]
    }
    Text(
        name,
        modifier = modifier.padding(bottom = 2.dp),
        style = MaterialTheme.typography.labelMedium,
        fontWeight = FontWeight.SemiBold,
        color = color,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
    )
}
