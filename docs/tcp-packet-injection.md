# TCP Packet Injection

## The TCP stack
Project-alpha maintains a single TCP connection, to the order flow connection on the exchange.

This TCP connection goes via the FPGA (acting as NIC), to the host.
- On the host, we do not use the linux TCP stack. Instead, on the host OS, we open a raw socket (therefore bypassing the linux networking stack), and connect this raw socket to the `smoltcp` rust TCP library. `smoltcp` then does most of the work maintaining the connection.
- However, the FPGA may autonomously "inject" outgoing TCP packets into this connection, when these packets need to be sent at low latency.

This injection behaviour creates some complications:
- Both sequence and ack numbers need to be co-ordinated between FPGA and host. This is made more complex because the FPGA needs to send packets immediately (ie, without first communicating with the host).
- The buffer of transmitted messages (needed for, eg, retransmission) is stored in `smoltcp`'s memory on the host. Messages sent autonomously by the FPGA need to be added to that buffer, both to maintain correct sequence numbers, and because the FPGA cannot do retransmission itself.

## DMA Toggle Functionality
A key addition to the design is the ability to enable/disable DMA sending on a per-socket basis:
- **DMA Enabled**: Data is sent via the FPGA using DMA, and the FPGA is allowed to send autonomous packets
- **DMA Disabled**: Data is sent via the normal ethernet interface, and the FPGA is prohibited from sending autonomous packets
- This toggle is essential during TCP connection handshaking, finishing, and retransmission scenarios where smoltcp needs direct control

## Smoltcp's architecture
- `smoltcp` keeps a `tx_buffer`, which is a ring buffer of pure payload data. This buffer contains both data that needs to be sent, and data that has already been sent (but not yet acked).
- At any time, `tx_buffer[0]` is the last un-acked byte sent. The variable `local_seq_no` stores which actual sequence number this byte is (and from this, the sequence numbers of all the other bytes in the ring buffer can be established).
- `remote_last_seq` stores the last (though not always maximum, if in the case of retransmission) sequence number sent. If the socket is in an idle state, then this is the sequence number of the last byte of the ring buffer (so the ring buffer has been fully sent).
- In order to send, `smoltcp` just adds payload data to the ring buffer. Then, in its event loop, smoltcp notices that `remote_last_seq` - `local_seq_no` is less than the length of the tx buffer, and starts sending (and, consequently, advancing `remote_last_seq`) until it's all caught up.
- When doing retransmission, it just sets `remote_last_seq` to the last non-missing packet, and so the event loop again notices that `remote_last_seq` - `local_seq_no` is less than the length of tx buffer, and naturally starts sending again (in this case, retransmitting, though it doesn't know that).

## How we will ultimately manage this:
- Connection set-up and handshaking will be done by `smoltcp`. After handshaking, the basic connection details (including initial sequence number and initial ack number) will be passed to the FPGA. From then on, both the host and FPGA will store these values separately, though they should naturally stay synchronized throughout.
- We will be sending packet payloads to the FPGA via DMA, and having the FPGA packetize them.
- Both sequence numbers and the ack number will need to be stored on the FPGA.
- But, we also need to maintain this tx_buffer of packets that have been sent and may need re-transmitting; this is in smoltcp memory, and the sequence numbers stored for this buffer also need to be correct. Retransmission will always be done by `smoltcp`, the FPGA will keep no record of packets sent.
- As a design decision, I don't think we want to be retroactively inserting data into this buffer, we only ever want to put new data on the top of the buffer (whether it is data that smoltcp wants to send, or data that the FPGA has already sent). This is a decision which then has a number of consequences:
- `smoltcp`'s natural way of sending data is to add it to this buffer, then wait for the event loop to spot that data has been added. If we're going to adhere to the no-retrospective-insertion rule, at the point at which we add host-generated packets to the buffer, we first need to check with the FPGA that it hasn't sent any new data itself. If the FPGA has, we need to put this FPGA-generated packet onto the buffer first.
- We will have a function with this kind of signature to check for FPGA-generated data: `send_data_via_dma(payload) -> already_sent_data`. This function returns any data that the FPGA has autonomously sent since the last time it was run, and this function sends the payload data to the FPGA, and then invokes an atomic operation on the FPGA, which (necessarily) stops other packets from being sent while it sends the data from `payload`.
- When data is sent in this way, we will mark it as "already transmitted" using the masked ring buffer. Otherwise smoltcp would naturally also send it via the ethernet interface. Both the packet(s), if any, that the FPGA autonomously sent, and the packet that `smoltcp` sent via the FPGA will be added to the masked ring buffer as "already transmitted", to prevent `smoltcp` from sending either of these itself.
- the `send_data_via_dma` function would need to be blocking (within `smoltcp`) and stop `smoltcp`s world until it has completed. It will take at least as long as it takes the FPGA to receive and then to send the payload, possibly longer if DMA is busy or the FPGA's sending something else already.
- This achieves all the desired goals of being latency-efficient, not getting any packets out of order, and getting the sequence and ack numbers right.
- Although sending of TCP data will not use the ethernet iface (and instead directly send the payloads via DMA), any sub-TCP traffic that `smoltcp` might use will still be sent via the ethernet iface.

## What the FPGA will actually be doing
What the FPGA actually does, behind-the-scenes, is irrelevant for `smoltcp`, but the most likely set-up we will use is:
- Whenever the FPGA autonomously sends a packet, it appends that packet to a queue (or ring buffer, or whatever), in a DMA region.
- It also increments an internal counter, indicating how many new items have been added to that queue since the last time the host read from it.
- When the host requests autonomous messages from the FPGA (during `send_data_via_dma`, or `set_dma_disabled`, etc), this internal counter is returned to the host, and then reset to zero.
- The host is then responsible for dequeueing that many items.
- This may involve waiting for the queue to be fully updated (which could be necessary if the internal counter message has been received before the queue writes were fully written).

## Implementation in `smoltcp`

### Core Components
- **`MaskedSocketBuffer`**: Enhanced `tx_buffer` that tracks which data should be transmitted vs. already sent
- **`InjectedSegmentSynchronizer` trait**: Defines the interface for FPGA communication with DMA control
- **`DmaError`**: Error type for DMA-specific operations
- **Multiple synchronizer implementations**: `NoFpgaSynchronizer`, `LengthMockSynchronizer`, `TestFpgaSynchronizer`

### InjectedSegmentSynchronizer API
```rust
pub trait InjectedSegmentSynchronizer {
    /// Send payload via DMA and retrieve any autonomously injected data
    /// Returns any data the FPGA sent since last call
    /// Returns an error if DMA is disabled
    fn send_data_via_dma(&mut self, payload: &[u8]) -> Result<Option<Vec<u8>>, DmaError>;

    /// Enable DMA and notify FPGA it can send autonomous packets
    /// Takes sequence_number and ack_number to sync FPGA with current TCP state
    fn enable_dma(
        &mut self,
        seq_num: TcpSeqNumber,
        ack_num: TcpSeqNumber,
    ) -> Result<(), DmaError>;

    /// Disable DMA and retrieve any pending FPGA-sent data
    fn disable_dma(&mut self) -> Result<Vec<u8>, DmaError>;

    /// Check for new autonomous data without disabling DMA
    /// Returns any data the FPGA sent since last call, or None if no new data
    fn get_new_autonomous_data(&mut self) -> Result<Option<Vec<u8>>, DmaError>;
}
```

### Public Socket API
```rust
impl TcpSocket {
    /// Enable DMA sending with current TCP sequence/ack state
    pub fn enable_dma(&mut self, seq_num: TcpSeqNumber, ack_num: TcpSeqNumber) -> Result<(), DmaError>;
    
    /// Disable DMA and retrieve any pending FPGA data
    pub fn disable_dma(&mut self) -> Result<Vec<u8>, DmaError>;
    
    /// Check current DMA status
    pub fn is_dma_enabled(&self) -> bool;
}
```

### Sending Logic Changes
- **`send_impl`** modified to check DMA status before sending
- **DMA Enabled Path**: Data sent via `send_data_via_dma()`, FPGA autonomous data inserted first, both marked as "already sent"
- **DMA Disabled Path**: Data sent via normal ethernet interface using existing smoltcp mechanisms
- **Automatic DMA Disable**: During retransmission, DMA is automatically disabled and any pending FPGA data is retrieved

### Error Handling
- **Safe API**: `send_data_via_dma()` returns error if called when DMA disabled
- **State Management**: FPGA is notified when it can/cannot send autonomous packets
- **Data Recovery**: When disabling DMA, any pending FPGA-sent data is retrieved and added to tx_buffer

## Implementation Scenarios

### DMA Enabled: FPGA sends, then smoltcp sends
1. FPGA autonomously sends packet A
2. smoltcp wants to send packet B
3. smoltcp calls `send_data_via_dma(B)` and learns about packet A
4. smoltcp adds packet A (marked as already sent) then packet B (marked as already sent) to `tx_buffer`
5. No packets sent via ethernet interface (all handled by FPGA)

### DMA Disabled: Normal ethernet sending
1. smoltcp wants to send packet B
2. DMA is disabled, so packet B is added to `tx_buffer` for normal transmission
3. smoltcp's normal dispatch mechanism sends packet B via ethernet interface
4. FPGA cannot send autonomous packets while DMA is disabled

### DMA Toggle During Retransmission
1. TCP timeout occurs, triggering retransmission
2. smoltcp automatically disables DMA via `set_dma_disabled()`
3. Any pending FPGA data is retrieved and added to tx_buffer
4. smoltcp handles retransmission via normal ethernet interface
5. DMA can be re-enabled after retransmission is complete

### Connection Lifecycle
- **Handshaking**: DMA disabled, smoltcp handles SYN/ACK via ethernet
- **Established**: DMA enabled with `enable_dma(seq_num, ack_num)` for low-latency data
- **Closing**: DMA disabled, smoltcp handles FIN/ACK via ethernet

## Testing Infrastructure
- **`TestFpgaSynchronizer`**: Captures packets sent to FPGA and simulates autonomous data
- **`LengthMockSynchronizer`**: Proof-of-concept using lengths instead of actual data
- **`NoFpgaSynchronizer`**: For systems without FPGA (always returns DMA errors)

## Key Design Decisions
- **Per-socket DMA control**: Each TCP socket has independent DMA state
- **Explicit enable/disable**: DMA must be explicitly enabled with sequence numbers
- **Safe API**: Operations that require DMA return errors when disabled
- **Data ordering**: FPGA autonomous data always inserted before user data in buffer
- **Retransmission safety**: DMA automatically disabled during retransmission

