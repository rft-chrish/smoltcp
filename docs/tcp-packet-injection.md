# TCP Packet Injection

## The TCP stack
Project-alpha maintains a single TCP connection, to the order flow connection on the exchange.

This TCP connection goes via the FPGA (acting as NIC), to the host.
- On the host, we do not use the linux TCP stack. Instead, on the host OS, we open a raw socket (therefore bypassing the linux networking stack), and connect this raw socket to the `smoltcp` rust TCP library. `smoltcp` then does most of the work maintaining the connection.
- However, the FPGA may autonomously "inject" outgoing TCP packets into this connection, when these packets need to be sent at low latency.

This injection behaviour creates some complications:
- Both sequence and ack numbers need to be co-ordinated between FPGA and host. This is made more complex because the FPGA needs to send packets immediately (ie, without first communicating with the host).
- The buffer of transmitted messages (needed for, eg, retransmission) is stored in `smoltcp`'s memory on the host. Messages sent autonomously by the FPGA need to be added to that buffer, both to maintain correct sequence numbers, and because the FPGA cannot do retransmission itself.

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
- We will have a function with this kind of signature to check for FPGA-generated data: `send_payload_to_fpga(payload) -> already_sent_data`. This function returns any data that the FPGA has autonomously sent since the last time it was run, and this function sends the payload data to the FPGA, and then invokes an atomic operation on the FPGA, which (necessarily) stops other packets from being sent while it sends the data from `payload`.
- When data is sent in this way, we will mark it as "already transmitted" using the masked ring buffer. Otherwise smoltcp would naturally also send it via the ethernet interface. Both the packet(s), if any, that the FPGA autonomously sent, and the packet that `smoltcp` sent via the FPGA will be added to the masked ring buffer as "already transmitted", to prevent `smoltcp` from sending either of these itself.
- the `send_data_to_fpga` function would need to be blocking (within `smoltcp`) and stop `smoltcp`s world until it has completed. It will take at least as long as it takes the FPGA to receive and then to send the payload, possibly longer if DMA is busy or the FPGA's sending something else already.
- This achieves all the desired goals of being latency-efficient, not getting any packets out of order, and getting the sequence and ack numbers right.
- Although sending of TCP data will not use the ethernet iface (and instead directly send the payloads via DMA), any sub-TCP traffic that `smoltcp` might use will still be sent via the ethernet iface.

## Changes that need to be made to `smoltcp`
- `tx_buffer` needs to be made into a masked ring buffer.
- We need to call `send_payload_to_fpga` from within `send_impl`. `send_impl` is the function that all other send functions ultimately call, so this means all TCP sending will be done via `send_payload_to_fpga`.
- When `send_payload_to_fpga` is called in this way, we need to also reset the retransmission timer.
- We will need another function `allow_fpga_sending(enable: Bool)` which tells the FPGA whether it is allowed to autonomously send. Autonomous sending needs to be temporarily disabled when retransmission is being done.
- `smoltcp` needs to be adjusted so that data that has been marked as "already transmitted" in the masked ring buffer, does not get transmitted via the normal transmission mechanisms in smoltcp's event loop.

## Some example scenarios:
### FPGA sends, then smoltcp sends
- FPGA sends packet A.
- smoltcp wants to send packet B. It sends packet B to FPGA to send via `send_payload_to_fpga`, and simultaneously learns about packet A.
- It adds packet A (with send_mask = False) and then packet B (with send_mask = False) to `tx_buffer`.
- `smoltcp` itself sends nothing via the ethernet interface (as all sending was done via the FPGA).
### FPGA sends, then smoltcp sends twice
- FPGA sends packet A.
- smoltcp wants to send packet B. It sends packet B to FPGA to send via `send_payload_to_fpga`, and simultaneously learns about packet A.
- It adds packet A (with send_mask = False) and then packet B (with send_mask = False) to `tx_buffer`. Nothing is sent via the ethernet interface. This blocks until complete.
- It then sends packet C. Again it runs `send_payload_to_fpga`. The FPGA has not sent any new packets in the interim, so no new packets are returned from this. Packet C is added to the `tx_buffer`, which now (correctly) has `packet A ++ packet B ++ packet C`.

## What a proof-of-concept of this would look like:
- The same as above, but instead of sending an actual payload to the FPGA via the `send_payload_to_fpga` function, we would send just the length of the intended payload. The FPGA would then mock a payload from this (ie, make the payload the * character for the appropriate length)
- Similarly, the return value from the function would be the length, not the contents, of any data that the FPGA autonomously sent.
- This preserves much of the same architecture whilst requiring simplified DMA capabilities.
- The function's signature would remain the same.

