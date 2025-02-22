#[derive(Debug, Default)]
pub struct FeeManager {
    num_inputs: [u8; 3],
    num_outputs: [u8; 3],
}

impl FeeManager {
    pub fn add_input(&mut self, pool: u8) -> u64 {
        let fee = self.fee();
        self.num_inputs[pool as usize] += 1;
        self.fee() - fee
    }

    pub fn add_output(&mut self, pool: u8) -> u64 {
        let fee = self.fee();
        self.num_outputs[pool as usize] += 1;
        self.fee() - fee
    }

    pub fn fee(&self) -> u64 {
        let t = self.num_inputs[0].max(self.num_outputs[0]);
        let s = {
            let o = if self.num_inputs[1] > 0 {
                // if any input
                self.num_outputs[1].max(2) // min 2 outputs
            } else {
                self.num_outputs[1]
            };
            self.num_inputs[1].max(o)
        };
        let o = if self.num_inputs[2] > 0 || self.num_outputs[2] > 0 {
            // padding min 2 actions
            self.num_inputs[2].max(self.num_outputs[2]).max(2)
        } else {
            0
        };
        let f = t + s + o;
        tracing::info!("fee: {t} {s} {o} -> {f}");
        f as u64 * 5_000
    }

    #[allow(dead_code)]
    fn min_actions_padding(a: u8) -> u8 {
        if a == 0 {
            0
        } else {
            a.max(2)
        }
    }
}
