package com.promtuz.chat.ui.screens

import android.content.Intent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.wrapContentSize
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.ShareIdentityVM
import com.promtuz.chat.ui.components.BackTopBar
import com.promtuz.chat.ui.components.IdentityQrCode
import com.promtuz.chat.utils.InviteLink
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.delay

@Composable
fun ShareIdentityScreen(
    viewModel: ShareIdentityVM,
    onScanned: (ByteArray) -> Unit,
) {
    val colors = MaterialTheme.colorScheme
    var showScanner by remember { mutableStateOf(false) }

    // Gate the QR + share link on discoverability: a link is useless until our
    // KeyPackage is published, so a brand-new user waits (PAIRING.md). Scanning
    // someone else's code doesn't need our KP, so that stays available.
    var discoverable by remember { mutableStateOf(CoreBridge.kpPublishReady()) }
    LaunchedEffect(Unit) {
        while (!discoverable) {
            delay(1000)
            discoverable = CoreBridge.kpPublishReady()
        }
    }

    Scaffold(
        topBar = { BackTopBar("Share Identity") }
    ) { innerPadding ->
        Box(
            Modifier
                .fillMaxSize()
                .padding(innerPadding)
                .background(colors.background),
        ) {
            Column(
                Modifier.fillMaxSize(),
                verticalArrangement = Arrangement.spacedBy(48.dp, Alignment.CenterVertically)
            ) {
                Box(
                    Modifier
                        .wrapContentSize()
                        .align(Alignment.CenterHorizontally)
                ) {
                    if (discoverable) {
                        IdentityQrCode(viewModel.qrData.collectAsState())
                    } else {
                        Column(
                            horizontalAlignment = Alignment.CenterHorizontally,
                            verticalArrangement = Arrangement.spacedBy(16.dp),
                        ) {
                            CircularProgressIndicator()
                            Text(
                                "Getting you discoverable…",
                                style = MaterialTheme.typography.titleMedium,
                            )
                            Text(
                                "Publishing your keys so others can add you.",
                                style = MaterialTheme.typography.bodyMedium,
                                color = colors.onSurfaceVariant,
                                textAlign = TextAlign.Center,
                            )
                        }
                    }
                }

                Column(
                    Modifier.fillMaxWidth(),
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(12.dp)
                ) {
                    ShareLinkButton(viewModel.qrData.collectAsState().value.takeIf { discoverable })
                    ScanQRButton(onClick = { showScanner = true })
                }
            }
        }
    }

    if (showScanner) {
        QrScannerSheet(
            onResult = { showScanner = false; onScanned(it) },
            onDismiss = { showScanner = false },
        )
    }
}


@Composable
private fun ColumnScope.ShareLinkButton(inviteBytes: ByteArray?, modifier: Modifier = Modifier) {
    val context = LocalContext.current

    OutlinedButton(
        onClick = {
            val bytes = inviteBytes ?: return@OutlinedButton
            val link = InviteLink.build(bytes)
            val send = Intent(Intent.ACTION_SEND).apply {
                type = "text/plain"
                putExtra(Intent.EXTRA_TEXT, link)
            }
            context.startActivity(Intent.createChooser(send, "Share invite link"))
        },
        enabled = inviteBytes != null,
        modifier = modifier
            .fillMaxWidth(0.8f)
            .align(Alignment.CenterHorizontally),
    ) {
        Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            Icon(
                painter = painterResource(R.drawable.i_link),
                contentDescription = "Share Link Icon"
            )

            Text(
                "Share link",
                textAlign = TextAlign.Center,
                style = MaterialTheme.typography.labelLargeEmphasized
            )
        }
    }
}


@Composable
private fun ColumnScope.ScanQRButton(onClick: () -> Unit, modifier: Modifier = Modifier) {
    Button(
        onClick = onClick,
        modifier = modifier
            .fillMaxWidth(0.8f)
            .align(Alignment.CenterHorizontally),
    ) {
        Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            Icon(
                painter = painterResource(R.drawable.i_qr_code_scanner),
                contentDescription = "QR Code Scanner Icon"
            )

            Text(
                "Scan QR Code",
                textAlign = TextAlign.Center,
                style = MaterialTheme.typography.labelLargeEmphasized
            )
        }
    }
}
