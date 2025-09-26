//! Trace-based testing infrastructure for TCP packet injection with FPGA simulation

use crate::socket::tcp::TcpSeqNumber;

#[derive(Debug, Clone)]
pub struct FpgaInternalState {
    seq_num: TcpSeqNumber,
    ack_num: TcpSeqNumber,
}

#[derive(Debug, Clone)]
pub enum FpgaState {
    /// FPGA starts writing details of an autonomously sent packet to DMA memory
    AutonomouslyWriting {
        internal_state: FpgaInternalState,
        write_data: u8,
        write_address: usize,
    },
    Idle,
}

// Each "message" is a write to a single cell of the DMA memory region
#[derive(Debug, Clone)]
pub enum PipeState {
    Transferring {
        output: Option<(usize, u8)>,
        in_transit: Vec<(usize, u8)>,
    },
    Empty,
}

#[derive(Debug, Clone)]
pub enum FpgaAction {
    StartWrite { seq: i32, ack: i32, data: u8, addr: usize },
    StayIdle,
    StopWriting,
    ContinueWriting,
}

#[derive(Debug, Clone)]
pub enum PipeAction {
    OutputPacket { index: usize },
    NoAction,
}

impl FpgaState {
    pub fn advance(&self, action: &FpgaAction) -> FpgaState {
        use FpgaState::*;
        use FpgaAction::*;
        
        match (self, action) {
            (Idle, StartWrite { seq, ack, data, addr }) => {
                AutonomouslyWriting {
                    internal_state: FpgaInternalState {
                        seq_num: TcpSeqNumber(*seq),
                        ack_num: TcpSeqNumber(*ack),
                    },
                    write_data: *data,
                    write_address: *addr,
                }
            }
            (Idle, StayIdle) => Idle,
            (Idle, StopWriting) => Idle, // No-op
            (Idle, ContinueWriting) => Idle, // No-op
            
            (writing @ AutonomouslyWriting { .. }, ContinueWriting) => writing.clone(),
            (AutonomouslyWriting { .. }, StopWriting) => Idle,
            (AutonomouslyWriting { .. }, StayIdle) => Idle, // Treat as stop
            (AutonomouslyWriting { .. }, StartWrite { seq, ack, data, addr }) => {
                // Can immediately start a new write
                AutonomouslyWriting {
                    internal_state: FpgaInternalState {
                        seq_num: TcpSeqNumber(*seq),
                        ack_num: TcpSeqNumber(*ack),
                    },
                    write_data: *data,
                    write_address: *addr,
                }
            }
        }
    }
}

impl PipeState {
    pub fn advance(&self, fpga_action: &FpgaAction, pipe_action: &PipeAction) -> PipeState {
        use PipeState::*;
        use FpgaAction::*;
        use PipeAction::*;
        
        match self {
            Empty => {
                // Only transition to Transferring if FPGA starts a write
                match fpga_action {
                    StartWrite { addr, data, .. } => {
                        Transferring {
                            in_transit: vec![(*addr, *data)],
                            output: None,
                        }
                    }
                    _ => Empty,
                }
            }
            Transferring { in_transit, .. } => {
                let mut new_in_transit = in_transit.clone();
                
                // Add new write only on StartWrite action
                if let StartWrite { addr, data, .. } = fpga_action {
                    new_in_transit.push((*addr, *data));
                }
                
                // Process pipe action
                let output = match pipe_action {
                    OutputPacket { index } if !new_in_transit.is_empty() => {
                        let safe_idx = (*index).min(new_in_transit.len() - 1);
                        Some(new_in_transit.remove(safe_idx))
                    }
                    _ => None,
                };
                
                if new_in_transit.is_empty() && output.is_none() {
                    Empty
                } else {
                    Transferring { in_transit: new_in_transit, output }
                }
            }
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::strategy::BoxedStrategy;
    
    // Generate sequences of actions upfront for reproducibility
    prop_compose! {
        fn action_sequences(len: usize)(
            // Generate FPGA actions
            fpga_actions in proptest::collection::vec(
                prop_oneof![
                    7 => Just(FpgaAction::StayIdle),
                    3 => (0i32..1000, 0i32..1000, any::<u8>(), 0usize..0x2000)
                        .prop_map(|(seq, ack, data, addr)| FpgaAction::StartWrite { seq, ack, data, addr }),
                    2 => Just(FpgaAction::StopWriting),
                    2 => Just(FpgaAction::ContinueWriting),
                ],
                len
            ),
            // Generate pipe actions
            pipe_actions in proptest::collection::vec(
                prop_oneof![
                    8 => Just(PipeAction::NoAction),
                    2 => (0usize..10).prop_map(|idx| PipeAction::OutputPacket { index: idx })
                ],
                len
            ),
        ) -> (Vec<FpgaAction>, Vec<PipeAction>) {
            (fpga_actions, pipe_actions)
        }
    }
    
    proptest! {
        #[test]
        fn test_simple_property_test(
            (fpga_actions, pipe_actions) in action_sequences(50),
        ) {
            let mut fpga = FpgaState::Idle;
            let mut pipe = PipeState::Empty;
            let mut total_writes = 0;
            let mut total_outputs = 0;
            
            for (fpga_action, pipe_action) in fpga_actions.iter().zip(pipe_actions.iter()) {
                // Count StartWrite actions
                if matches!(fpga_action, FpgaAction::StartWrite { .. }) {
                    total_writes += 1;
                }
                
                // Advance both state machines
                fpga = fpga.advance(fpga_action);
                pipe = pipe.advance(fpga_action, pipe_action);
                
                // Count outputs
                if let PipeState::Transferring { output: Some(_), .. } = &pipe {
                    total_outputs += 1;
                }
            }
            
            // Properties
            prop_assert!(total_outputs <= total_writes, "Cannot output more than written");
            
            if let PipeState::Transferring { in_transit, .. } = &pipe {
                prop_assert_eq!(
                    in_transit.len(),
                    (total_writes - total_outputs) as usize,
                    "Queue length should match writes - outputs"
                );
            }
        }
    }
}
