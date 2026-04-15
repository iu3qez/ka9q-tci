# Network discovery — radiod multicast su RPi5

Risultati della sessione di discovery del 2026-04-15 sul Pi5 con RX888 MkII.

## Setup di riferimento

| Componente | Valore |
|---|---|
| Host | Raspberry Pi 5 (aarch64), Debian/Bookworm |
| SDR | RX888 MkII (Cypress FX3, USB 3.0 SuperSpeed) |
| Interfaccia rete | `eth0` 192.168.1.228, `wlan0` down |
| radiod | `radiod@rx888-generic.service`, config `/etc/radio/radiod@rx888-generic.conf` |
| Sample rate ADC | 64.8 MHz |
| Canali attivi | 28 (17 WSPR + 10 FT8 + 1 HF Manual) |

## Gruppi multicast rilevati

Risolti via mDNS (`getent hosts`):

| Nome mDNS | Indirizzo multicast | Ruolo |
|---|---|---|
| `hf.local` | `239.135.38.120` | **Status/command** del receiver (porta 5006) |
| `ft8-pcm.local` | `239.22.92.109` | Data PCM canali FT8 (porta 5004) |
| `wspr-pcm.local` / `rx888-generic-pcm.local` | `239.107.139.201` | Data PCM canali WSPR (porta 5004) |
| `hf-pcm.local` | `239.206.102.211` | Data PCM canale HF Manual (porta 5004) |

### Porte

- **5004** — RTP data (PCM / IQ payload)
- **5006** — control plane (COMMAND in, STATUS out), stesso gruppo del data
  corrispondente oppure gruppo receiver-level (`hf.local`)

## Problemi incontrati e soluzioni

### 1. RX888 non inizializza (USB speed "High 480 Mb/s")

**Sintomo**: `radiod` in crash-loop (67+ restart), log:
```
USB speed: High (480 Mb/s): not at least SuperSpeed; is it plugged into a blue USB jack?
rx888_usb_init() failed
```

**Causa**: race condition. Il crash-loop rapido non dava tempo al bus USB
di stabilizzarsi. A livello kernel (`lsusb -t`, `/sys/bus/usb/devices/4-1/speed`)
il device era correttamente a 5000M (SuperSpeed), ma `libusb_get_device_speed()`
dentro al crash-loop restituiva High Speed.

**Nota**: l'RX888 MkII enumera inizialmente come USB 2.0 (PID `0x04b4:0x00f3`)
prima del caricamento firmware FX3. Dopo il firmware upload (`SDDC_FX3.img` via
`ezusb_load_ram()`), re-enumera come PID `0x04b4:0x00f1` a SuperSpeed.
Se il device appare gia come `0x00f1` ma a High Speed, e un residuo del
firmware in RAM FX3 dopo un reset incompleto.

**Soluzione**: fermare il servizio (`systemctl stop`), attendere che il bus
si stabilizzi, riavviare una sola volta. In casi estremi: unbind/rebind USB
o power cycle fisico.

### 2. Multicast TTL = 0 (default) — nessun pacchetto su eth0

**Sintomo**: `radiod` running, canali attivi, ma zero pacchetti visibili
su `eth0`. `metadump` non riceve nulla. Listener Python muto.

**Causa**: il default di ka9q-radio e `ttl = 0`
(`src/modes.c`: `DEFAULT_TTL = 0`), documentato come
*"Don't blast cheap switches and access points unless the user says so"*.
Con TTL=0 tutto il traffico multicast resta su loopback (`lo`).

**Verifica**:
```bash
# Traffico visibile solo su lo:
sudo tcpdump -i lo -c 5 dst net 239.0.0.0/8 -n   # OK, pacchetti
sudo tcpdump -i eth0 -c 5 dst net 239.0.0.0/8 -n  # zero
```

**Soluzione**: in `/etc/radio/radiod@rx888-generic.conf`, sezione `[global]`:
```ini
ttl = 1    # 1 = LAN only (non attraversa router)
```
Poi `sudo systemctl restart radiod@rx888-generic.service`.

### 3. Multi-homing eth0 + wlan0

**Sintomo** (osservato prima di disattivare wlan0): con due interfacce UP
e due default route, il kernel/IGMP puo scegliere l'interfaccia sbagliata
per il join multicast (`INADDR_ANY` non e deterministico).

**Soluzione temporanea**: `wlan0` down (`ip link set wlan0 down`).

**Soluzione permanente** (da implementare nel bridge Rust): specificare
esplicitamente l'interfaccia nel join multicast con `IP_ADD_MEMBERSHIP`
usando l'indirizzo di `eth0` anziché `INADDR_ANY`. In ka9q-radio si puo
anche impostare `iface = eth0` nella config.

## Stato verificato post-fix

```
# Data plane — pacchetti RTP su eth0:
sudo tcpdump -i eth0 -c 5 dst net 239.0.0.0/8 port 5004 -n  # OK

# Control plane — STATUS ricevibili:
python3 listener su 239.135.38.120:5006  # OK, ~10 pkt/burst, 296B ciascuno

# Heartbeat STATUS spontanei per canale:
239.22.92.109:5006   (ft8-pcm)   # OK
239.107.139.201:5006 (wspr-pcm)  # OK
```

## Comandi utili per diagnostica

```bash
# Stato servizio
systemctl status radiod@rx888-generic.service
journalctl -u radiod@rx888-generic.service -n 30 --no-pager

# USB
lsusb -t                              # velocita device
cat /sys/bus/usb/devices/4-1/speed     # kernel-level speed

# Multicast membership
ip maddr show dev eth0 | grep 239

# Socket in ascolto
ss -ulnp | grep 5006

# Sniff
sudo tcpdump -i eth0 -c 10 dst net 239.0.0.0/8 -n
sudo tcpdump -i any -c 10 host 239.135.38.120 -n

# mDNS resolve
getent hosts hf.local
getent hosts ft8-pcm.local

# Listener Python veloce (control plane)
python3 -c "
import socket, struct, time
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('', 5006))
s.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP,
    struct.pack('4s4s', socket.inet_aton('239.135.38.120'),
                        socket.inet_aton('0.0.0.0')))
s.settimeout(3)
while True:
    data, addr = s.recvfrom(4096)
    print(f'{len(data)}B from {addr}')
"
```

## Implicazioni per ka9q-tci

1. **Il bridge deve joinare i gruppi multicast specificando l'interfaccia**
   (`IP_ADD_MEMBERSHIP` con indirizzo esplicito, non `INADDR_ANY`),
   configurabile da CLI/file.

2. **Il control plane e request/response**: radiod emette STATUS spontanei
   (heartbeat) sui gruppi per-canale, ma sul gruppo receiver-level
   (`hf.local`) risponde solo a POLL espliciti.

3. **Il data plane RTP arriva su porta 5004**, il control/status su **5006**,
   sugli stessi gruppi multicast.

4. **TTL=1 e prerequisito** nella config di radiod per qualsiasi uso LAN.
   Documentarlo nei requisiti di setup.
