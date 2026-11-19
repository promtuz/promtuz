package com.promtuz.chat.ui.components

import androidx.annotation.DrawableRes
import androidx.compose.foundation.background
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.ui.unit.Dp
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.R
import com.promtuz.chat.ui.text.ttCommonsProFamily
import com.promtuz.chat.ui.theme.Promtuz
import kotlinx.coroutines.flow.MutableStateFlow
import uniffi.core.HiddenTag

/** The chat just hidden in this session, so it can confirm what happened once. */
object HiddenNotice {
    val conversation = MutableStateFlow<String?>(null)
}

/** A round badge on an avatar's corner, cut out of the background like a presence dot. */
@Composable
fun AvatarBadge(avatarSize: Dp, modifier: Modifier = Modifier, content: @Composable () -> Unit) {
    Box(
        modifier.offset(4.dp, 4.dp).size(avatarSize * 0.375f).clip(CircleShape)
            .background(MaterialTheme.colorScheme.background).padding(3.dp).clip(CircleShape).background(Promtuz.colors.card),
        contentAlignment = Alignment.Center,
    ) { content() }
}

@Composable
fun AvatarBadge(@DrawableRes icon: Int, avatarSize: Dp, modifier: Modifier = Modifier) =
    AvatarBadge(avatarSize, modifier) { DrawableIcon(icon, size = avatarSize * 0.2f, tint = Promtuz.colors.muted) }

@Composable
fun TagChip(tag: HiddenTag, modifier: Modifier = Modifier) {
    Box(
        modifier.size(40.dp, 24.dp).clip(RoundedCornerShape(8.dp)).background(avatarPalette(tag.color.toInt())),
        contentAlignment = Alignment.Center,
    ) {
        Text(tag.code, color = Color.White, fontSize = 13.sp, fontWeight = FontWeight.Medium, letterSpacing = 0.5.sp)
    }
}

@Composable
fun HideChatSheet(name: String, busy: Boolean, onHide: () -> Unit, onDismiss: () -> Unit) {
    val c = Promtuz.colors
    AppBottomSheet(onDismissRequest = { if (!busy) onDismiss() }, sheetState = rememberModalBottomSheetState(true), dismissEnabled = !busy) {
        Column(
            Modifier.fillMaxWidth().padding(horizontal = 17.dp).padding(top = 12.dp, bottom = 24.dp).navigationBarsPadding(),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Box(Modifier.size(64.dp).clip(RoundedCornerShape(20.dp)).background(cardFill()), contentAlignment = Alignment.Center) {
                DrawableIcon(R.drawable.oi_hidden_chat, size = 36.dp, tint = c.accent)
            }
            Spacer(Modifier.height(16.dp))
            Text("Hide chat", style = SheetTitleStyle, color = c.text)
            Spacer(Modifier.height(8.dp))
            Text(
                "$name and this chat leave this phone until you enter your code in search. $name isn't told.",
                style = TextStyle(fontSize = 15.sp, lineHeight = 21.sp), color = c.muted, textAlign = TextAlign.Center,
            )
            Spacer(Modifier.height(24.dp))
            AppButton("Hide chat", onHide, Modifier.fillMaxWidth().height(52.dp), busy = busy, enabled = !busy)
        }
    }
}

private val SheetTitleStyle = TextStyle(fontFamily = ttCommonsProFamily, fontWeight = FontWeight.SemiBold, fontSize = 24.sp, lineHeight = 30.sp)
