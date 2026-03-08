use crate::runtime::{
    manager::{assert::TestTransReadState, Manager}, 
    message::Msg, 
    transaction::TxnId, TestId
};
use std::error::Error;


impl Manager {
    /// 1. first step process assertions is to obtain all (latest) transactions
    /// it's testing against
    pub async fn request_assertion_preds(&mut self, test_id: TestId)
    -> Result<(), Box<dyn Error>> {
        let test_mgr = self.test_mgrs.get(&test_id).unwrap();

        for (name, state) in test_mgr.trans_reads.iter() {
            assert!(*state == TestTransReadState::Requested);

            self.tell_to_name(name, 
                Msg::TestRequestPred {
                    test_id,
                    from_mgr_addr: self.address.clone().unwrap(),
                }
            ).await?;
        }

        Ok(())
    }

    /// 2. second step is request value to the def actor representing the test
    pub async fn request_assertion_result(
        &self, 
        test_id: TestId
    ) -> Result<(), Box<dyn Error>> {
        assert!(self.all_pred_granted(test_id));

        let test_mgr = self.test_mgrs.get(&test_id).unwrap();
        let mut preds = vec![];
        for (_name , state) in test_mgr.trans_reads.iter() {
            assert!(*state != TestTransReadState::Requested);
            if let TestTransReadState::Depend(Some(pred_id)) = state {
                preds.push(pred_id.clone());
            }
        }

        test_mgr.assert_actor.tell(
            Msg::TestReadDefRequest { 
                from_mgr_addr: self.address.clone().unwrap(), 
                test_id, 
                preds
            }).await?;
        
        Ok(())
    }
}


impl Manager {
    pub fn add_grant_pred(&mut self, test_id: TestId, name: String, pred: Option<TxnId>) {
        let test_mgr = self.test_mgrs.get_mut(&test_id).unwrap();
        test_mgr.add_grant_pred(name, pred);
    }

    pub fn all_pred_granted(&self, test_id: TestId) -> bool {
        let test_mgr = self.test_mgrs.get(&test_id).unwrap();
        test_mgr.all_pred_granted()
    }
}