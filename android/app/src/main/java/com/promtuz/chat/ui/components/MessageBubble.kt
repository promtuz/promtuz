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
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import com.promtuz.chat.ui.text.EmojiText
import com.promtuz.chat.ui.text.MessageText
import com.promtuz.chat.ui.text.MessageLinkGestures
import com.promtuz.chat.ui.text.LocalMessageLinkGestures
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
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
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.boundsInRoot
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.text.TextLayoutResult
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.dp
import kotlin.math.ceil
import com.promtuz.chat.domain.model.MessageContent
import androidx.compose.ui.text.font.FontWeight
import com.promtuz.chat.domain.model.Quote
import com.promtuz.chat.domain.model.ReactionGroup
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.ui.appearance.LocalChatAppearance
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.stage.ChatMotion
import android.os.Build

internal val BubblePadH = 11.dp
internal val BubblePadV = 6.dp

/** Lowers a meta on the last line below the baseline, within [BubblePadV] so the bubble never grows. */
private val MetaBaselineDrop = 3.dp

private val MetaHaloBlur = 22.dp

private const val MetaHaloSpreadX = 2.6f
private const val MetaHaloSpreadY = 3.4f

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
    onTap: (() -> Unit)? = null,
    onMediaTap: ((String) -> Unit)? = null,
    onDownload: ((String) -> Unit)? = null,
    onOpen: ((String) -> Unit)? = null,
    onSenderClick: (() -> Unit)? = null,
    peerName: String = "",
) {
    val appearance = LocalChatAppearance.current
    val chat = LocalChatColors.current
    val outgoing = msg.outgoing
    val showSender = msg.senderName != null && !outgoing && !mergedTop
    val groupIncoming = msg.senderName != null && !outgoing
    val bubbleColor = if (outgoing) chat.outgoingBubble else chat.incomingBubble
    val textColor = if (outgoing) chat.onOutgoingBubble else chat.onIncomingBubble
    val metaLabel = BubbleTextLayouts.metaLabelOf(LocalContext.current, msg)
    val haptic = LocalHapticFeedback.current
    // Plain refs, not snapshot state: positions change every frame and are read
    // only in gesture handlers and the measure pass that wrote them.
    val coords = remember { CoordsHolder() }
    // pointerInput keys only on menuState, so its coroutine would keep the first onLongPress.
    val longPress by rememberUpdatedState(onLongPress)
    val isTextBlock = msg.deleted || msg.content is MessageContent.Text

    // Emoji, voice notes and stickers omit the bubble unless a quote needs it.
    val jumboEmoji = !msg.deleted && msg.quote == null &&
        (msg.content as? MessageContent.Text)?.let { isJumboEmoji(it.text) } == true
    val bare = jumboEmoji || (!msg.deleted && msg.quote == null &&
        (msg.content is MessageContent.Voice || msg.content is MessageContent.Sticker))

    // A picture runs to the bubble's edge, so the bubble waives its inset and the padded
    // blocks put it back themselves. An attachment card keeps the inset like text.
    val tile = (msg.content as? MessageContent.Attachment)?.isMediaTile == true
    val bleeds = !msg.deleted &&
        (msg.content is MessageContent.Image || msg.content is MessageContent.Album || tile)
    val caption = when (val c = msg.content) {
        is MessageContent.Image -> c.caption
        is MessageContent.Album -> c.caption
        is MessageContent.Attachment -> if (tile) c.caption else ""
        else -> ""
    }
    // With nothing below it to sit in, the time has to ride the picture itself.
    val stickerAlone = !msg.deleted && msg.quote == null && msg.content is MessageContent.Sticker
    val metaOnMedia = (bleeds || stickerAlone) && caption.isEmpty() && msg.reactions.isEmpty()
    // A borderless picture has no body for a tail to grow from, so it goes without one.
    val shape = rememberBubbleShape(
        outgoing, mergedTop, mergedBottom,
        if (metaOnMedia) appearance.bubble.copy(tail = false) else appearance.bubble,
    )

    // Not BoxWithConstraints: that's a SubcomposeLayout per bubble. The Layout below applies the width cap.
    val widthFraction = appearance.layout.maxWidthFraction
    val linkGestures = LocalMessageLinkGestures.current ?: remember { MessageLinkGestures() }
    Box(
        modifier
            .fillMaxWidth()
            .onGloballyPositioned { coords.row = it }
            .padding(horizontal = 12.dp),
        contentAlignment = if (outgoing) Alignment.CenterEnd else Alignment.CenterStart,
    ) {
        if (groupIncoming && !mergedBottom) {
            Box(Modifier.align(Alignment.BottomStart)) {
                Avatar(
                    msg.senderName.orEmpty(), size = 30.dp,
                    identityKey = msg.senderHex ?: msg.senderName.orEmpty(),
                    image = com.promtuz.chat.utils.media.rememberAvatar(msg.senderHex),
                    onClick = onSenderClick,
                )
            }
        }
        Layout(
            content = {
                if (showSender) {
                    SenderLabel(
                        msg.senderName ?: "Unknown",
                        msg.senderHex,
                        onClick = onSenderClick,
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

                // The Layout reads children by index, so every variant emits exactly one measurable.
                CompositionLocalProvider(LocalMessageLinkGestures provides linkGestures) {
                    val content = msg.content
                    when {
                        msg.deleted || content is MessageContent.Text ->
                            BubbleText(
                                msg, textColor,
                                appearance.type.fontScale * (if (jumboEmoji) JumboEmojiScale else 1f),
                            ) { coords.text = it }
                        content is MessageContent.Image ->
                            ImageBlock(
                                content, textColor, appearance.type.fontScale,
                                metaLabel, outgoing,
                                originKey = msg.dispatchIdHex,
                                onOpen = msg.dispatchIdHex?.let { did -> onMediaTap?.let { cb -> { cb(did) } } },
                            )
                        content is MessageContent.Album ->
                            AlbumBlock(
                                content, textColor, appearance.type.fontScale,
                                metaLabel, outgoing,
                                onOpen = onMediaTap,
                            )
                        content is MessageContent.Attachment && tile ->
                            MediaTileBlock(
                                content, textColor, appearance.type.fontScale,
                                metaLabel, outgoing,
                                originKey = msg.dispatchIdHex, onDownload = onDownload,
                                onOpen = msg.dispatchIdHex?.let { did -> onMediaTap?.let { cb -> { cb(did) } } },
                            )
                        content is MessageContent.Attachment ->
                            AttachmentBlock(
                                content, textColor, appearance.type.fontScale,
                                metaLabel, peerName, outgoing, onDownload, onOpen,
                            )
                        content is MessageContent.Voice ->
                            VoiceBlock(content, textColor, surface = if (bare) bubbleColor else null)
                        content is MessageContent.Sticker ->
                            StickerBlock(content, textColor)
                    }
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

                MetaRow(msg, metaLabel, textColor, metaOnMedia, pill = if (bare && !metaOnMedia) bubbleColor else null)
            },
            modifier = Modifier
                .then(if (groupIncoming) Modifier.padding(start = 40.dp) else Modifier)
                .typingMorphSurface(shape, bubbleColor, textColor, enabled = !bare)
                // The surface fills before animateContentSize and .clip, which clip to the
                // node's rectangle and would shear off the tail.

                // Resizes run on the shared clock so the stage moves neighbors in lockstep.
                .animateContentSize(
                    ChatMotion.spec(),
                    alignment = if (outgoing) Alignment.BottomEnd else Alignment.BottomStart,
                )
                .then(if (bare) Modifier else Modifier.clip(shape))
                .onGloballyPositioned { coords.bubble = it }
                .then(
                    if (onLongPress == null) Modifier
                    else Modifier.pointerInput(menuState) {
                        awaitEachGesture {
                            val down = awaitFirstDown(requireUnconsumed = false)
                            coords.longPressed = false
                            if (linkGestures.owns(down)) return@awaitEachGesture
                            if (menuState?.isOpen == true) return@awaitEachGesture
                            awaitLongPressOrCancellation(down.id) ?: return@awaitEachGesture
                            // The tap detector below sees this same release later and
                            // must not treat it as a tap on top of the menu.
                            coords.longPressed = true
                            haptic.performHapticFeedback(HapticFeedbackType.LongPress)
                            longPress?.invoke(
                                coords.row?.takeIf { it.isAttached }?.boundsInRoot() ?: Rect.Zero
                            )
                            if (menuState != null) dragSelect(
                                down, haptic, { menuState.hitIndex(coords.bubble, it) },
                                { menuState.hovered = it }, menuState::pick,
                            )
                        }
                    }
                )
                .then(
                    // One detector for both, so a sticker's tap waits out the
                    // double-tap window instead of stealing it.
                    if (onDoubleTap == null && onTap == null) Modifier
                    else Modifier.pointerInput(onDoubleTap, onTap) {
                        detectTapGestures(
                            onDoubleTap = onDoubleTap?.let { cb -> { cb() } },
                            onTap = onTap?.let { cb -> { if (!coords.longPressed) cb() } },
                        )
                    }
                )
                .padding(
                    horizontal = if (bleeds || bare) 0.dp else BubblePadH,
                    vertical = if (bleeds || bare) 0.dp else BubblePadV,
                ),
        ) { measurables, constraints ->
            // Children: [sender?] [quote?] content [reactions?] meta.
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

            val lastLine = coords.text
                ?.takeIf { it.lineCount > 0 }
                ?.let { ceil(it.getLineRight(it.lineCount - 1)).toInt() }

            val metaGap = 8.dp.roundToPx()
            // The meta rides the last line when it fits, else takes a meta-height row of its own.
            // Reactions share their line with the meta instead.
            var metaRow = 0
            var metaDrop = 0
            val contentWidth = when {
                // Nothing to ride: the pill sits under the content, at its end.
                bare && !metaOnMedia -> { metaRow = meta.height + BarePillGap.roundToPx(); maxOf(text.width, meta.width) }
                hasReactions -> maxOf(text.width, reactions!!.width + metaGap + meta.width)
                // Media owns its whole footprint and keeps the corner clear itself.
                !isTextBlock -> text.width
                // No layout to consult: give the meta its own row rather than risk a glyph.
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
            // metaDrop is left out: it hangs into the padding, while metaRow needs real space.
            val height = (sender?.height ?: 0) + (quote?.height ?: 0) + text.height + metaRow +
                (reactions?.height ?: 0)
            // Bare: reactions go under the content and the pill under them.
            val reactionsY = (sender?.height ?: 0) + (quote?.height ?: 0) + text.height
            layout(width, height) {
                var y = 0
                sender?.let { it.placeRelative(0, y); y += it.height }
                quote?.let { it.placeRelative(0, y); y += it.height }
                // Bare content hugs the tail side, like the pill under it.
                val tx = if (bare && outgoing) width - text.width else 0
                text.placeRelative(tx, y)
                reactions?.placeRelative(if (bare && outgoing) width - reactions.width else 0, reactionsY)
                // A bleeding bubble has no padding, so the meta takes the same inset itself.
                val metaInsetX = if (bleeds || metaOnMedia) BubblePadH.roundToPx() else 0
                val metaInsetY = if (bleeds || metaOnMedia) BubblePadV.roundToPx() else 0
                meta.placeRelative(
                    width - meta.width - metaInsetX + metaDrop,
                    height - meta.height - metaInsetY + metaDrop,
                )
            }
        }
    }
}

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
        EmojiText(rg.emoji, style = MaterialTheme.typography.labelMedium)
        if (rg.count > 1) Text(
            " ${rg.count}",
            style = MaterialTheme.typography.labelSmall,
            color = textColor.copy(alpha = 0.85f),
        )
    }
}

/** Reserves nothing for the meta; [onLayout] fires during measure, so the bubble reads it in the same pass. */
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
    MessageText(
        AnnotatedString(text),
        Modifier.fadeOnChange(text),
        style = base.copy(fontSize = base.fontSize * fontScale, color = color,
            fontStyle = if (msg.deleted) FontStyle.Italic else FontStyle.Normal),
        color = color,
        onTextLayout = onLayout,
    )
}

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

@Composable
private fun MetaRow(
    msg: UiMessage, label: String, textColor: Color, onMedia: Boolean = false, pill: Color? = null,
) {
    val metaStyle = MaterialTheme.typography.labelSmall
    val metaColor = if (onMedia) Color.White else textColor.copy(alpha = if (pill != null) 0.8f else 0.55f)
    val accent = LocalChatColors.current.accent
    val error = MaterialTheme.colorScheme.error

    Box(
        if (pill != null) Modifier.clip(CircleShape).background(pill).padding(horizontal = 8.dp, vertical = 3.dp)
        else Modifier,
        contentAlignment = Alignment.Center,
    ) {
        if (onMedia) MetaHalo(Modifier.matchParentSize())
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                label,
                style = metaStyle,
                color = metaColor,
                maxLines = 1,
                softWrap = false,
            )
            if (msg.outgoing) key(msg.localId) {
                MessageStatusIcon(
                    status = msg.status,
                    modifier = Modifier.padding(start = BubbleStatusGap).size(BubbleStatusSize),
                    tint = metaColor,
                    seenTint = if (onMedia) lerp(accent, Color.White, 0.55f) else accent,
                    errorTint = if (onMedia) lerp(error, Color.White, 0.35f) else error,
                )
            }
        }
    }
}

/** `Modifier.blur` is a no-op below API 31, so a radial ramp stands in there. */
@Composable
private fun MetaHalo(modifier: Modifier) {
    val shaped = modifier.graphicsLayer {
        scaleX = MetaHaloSpreadX
        scaleY = MetaHaloSpreadY
        // Grows into the bottom-end corner, where the bubble's clip trims it.
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

private const val JumboEmojiScale = 2.8f
private val BarePillGap = 4.dp
private const val JumboEmojiMax = 3

private val EmojiOnly = Regex(
    "^(?:\\p{So}|\\p{Cn}|[\\uFE0F\\u200D\\u20E3]|[\\x{1F3FB}-\\x{1F3FF}]|[\\x{1F1E6}-\\x{1F1FF}])+$",
)

internal fun isJumboEmoji(text: String): Boolean {
    val t = text.trim()
    if (t.isEmpty() || !EmojiOnly.matches(t)) return false
    val it = java.text.BreakIterator.getCharacterInstance()
    it.setText(t)
    var n = 0
    while (it.next() != java.text.BreakIterator.DONE) if (++n > JumboEmojiMax) return false
    return n in 1..JumboEmojiMax
}

private class CoordsHolder {
    var row: LayoutCoordinates? = null
    var bubble: LayoutCoordinates? = null

    /** The current gesture opened the menu; its release is not a tap. */
    var longPressed = false

    /** Last text layout, written during the text child's measure and read right after it. */
    var text: TextLayoutResult? = null
}

@Composable
private fun SenderLabel(name: String, key: String?, modifier: Modifier = Modifier, onClick: (() -> Unit)? = null) {
    val palette = LocalChatColors.current.senderPalette
    val color = remember(key, palette) {
        palette[Math.floorMod(key?.hashCode() ?: 0, palette.size)]
    }
    Text(
        name,
        modifier = modifier.then(if (onClick != null) Modifier.clickable(onClick = onClick) else Modifier).padding(bottom = 2.dp),
        style = MaterialTheme.typography.labelMedium,
        fontWeight = FontWeight.SemiBold,
        color = color,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
    )
}
