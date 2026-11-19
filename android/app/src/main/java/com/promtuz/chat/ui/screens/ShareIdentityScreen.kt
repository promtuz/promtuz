package com.promtuz.chat.ui.screens

import android.content.ClipData
import android.content.Intent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.ClipEntry
import androidx.compose.ui.platform.LocalClipboard
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.ProfileVM
import com.promtuz.chat.presentation.viewmodel.ShareIdentityVM
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.text.ttCommonsProFamily
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.theme.Radii
import com.promtuz.chat.utils.InviteLink
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

private val NameStyle = TextStyle(fontFamily = ttCommonsProFamily, fontWeight = FontWeight.SemiBold, fontSize = 22.sp, lineHeight = 28.sp)
private val KeyStyle = TextStyle(fontFamily = FontFamily.Monospace, fontSize = 13.sp, lineHeight = 19.sp)

@Composable
fun ShareIdentityScreen(
    viewModel: ShareIdentityVM,
    profileVM: ProfileVM,
    onScanned: (ByteArray) -> Unit,
) {
    val c = Promtuz.colors
    val context = LocalContext.current
    val clipboard = LocalClipboard.current
    val scope = rememberCoroutineScope()
    val profile by profileVM.profile.collectAsState()
    val qr by viewModel.qrData.collectAsState()
    var showScanner by remember { mutableStateOf(false) }
    LaunchedEffect(Unit) { profileVM.refresh() }

    // The QR and link are useless until our KeyPackage is published, so they wait for it.
    var discoverable by remember { mutableStateOf(false) }
    LaunchedEffect(Unit) {
        while (true) {
            discoverable = withContext(Dispatchers.IO) { CoreBridge.kpPublishReady() }
            if (discoverable) break
            delay(1000)
        }
    }
    val invite = qr.takeIf { discoverable }

    GroupedScreen({ Text("My QR code") }, connectionStatus = false) { padding ->
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(padding)
                .padding(horizontal = 24.dp, vertical = 8.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Box(contentAlignment = Alignment.TopCenter) {
                Column(
                    Modifier.padding(top = 44.dp).fillMaxWidth().clip(RoundedCornerShape(Radii.modal)).background(cardFill())
                        .padding(start = 20.dp, end = 20.dp, top = 56.dp, bottom = 20.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                ) {
                    Text(profile.name, style = NameStyle, color = c.text, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    if (profile.bio.isNotBlank()) Text(profile.bio, style = RowSubtitleStyle, color = c.muted,
                        maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Spacer(Modifier.height(16.dp))
                    Box(Modifier.widthIn(max = 246.dp).fillMaxWidth(0.8f), contentAlignment = Alignment.Center) {
                        if (discoverable) IdentityQrCode(viewModel.qrData.collectAsState(), Modifier.fillMaxWidth())
                        else Box(Modifier.fillMaxWidth().aspectRatio(1f).clip(RoundedCornerShape(Radii.modal))
                            .background(c.cardHigh), Alignment.Center) {
                            CircularProgressIndicator(Modifier.size(28.dp), color = c.muted, strokeWidth = 2.5.dp)
                        }
                    }
                    Spacer(Modifier.height(16.dp))
                    Text(if (discoverable) "Scan to message me on Promtuz" else "Connecting so people can add you…",
                        style = FootnoteStyle, color = c.muted, textAlign = TextAlign.Center)
                }
                Avatar(profile.name.ifBlank { "?" }, 88.dp, identityKey = profile.identity.ifBlank { profile.name }, image = profile.picture)
            }
            if (profile.identity.isNotEmpty()) {
                Spacer(Modifier.height(16.dp))
                SettingsCard(header = "Identity key") {
                    Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp, bottom = 12.dp),
                        verticalAlignment = Alignment.CenterVertically) {
                        Text(profile.identity.uppercase().chunked(4).chunked(4).joinToString("\n") { it.joinToString("  ") },
                            Modifier.weight(1f).padding(top = 4.dp), style = KeyStyle, color = c.text)
                        AppIconButton(R.drawable.oi_copy, "Copy identity key", {
                            scope.launch { clipboard.setClipEntry(ClipEntry(ClipData.newPlainText("Promtuz identity", profile.identity))) }
                        }, tint = c.muted)
                    }
                }
            }
            Spacer(Modifier.weight(1f).heightIn(min = 24.dp))
            AppButton("Share invite link", onClick = {
                val bytes = invite ?: return@AppButton
                context.startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND).apply {
                    type = "text/plain"
                    putExtra(Intent.EXTRA_TEXT, InviteLink.build(bytes))
                }, "Share invite link"))
            }, enabled = invite != null, modifier = Modifier.fillMaxWidth(), icon = R.drawable.oi_export)
            Spacer(Modifier.height(8.dp))
            AppButton("Scan a code", onClick = { showScanner = true }, modifier = Modifier.fillMaxWidth(),
                kind = ButtonKind.Tonal, icon = R.drawable.oi_qr_code_scanner)
        }
    }

    if (showScanner) QrScannerSheet(
        onResult = { showScanner = false; onScanned(it) },
        onDismiss = { showScanner = false },
    )
}
